use crate::{
    desktop::ApplicationInfo,
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
const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
#[cfg(windows)]
const MAX_PATH: usize = 260;
#[cfg(windows)]
const CLOSE_SETTLE: Duration = Duration::from_millis(1_500);

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
        self.failures.is_empty()
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
    value
        .rsplit(['\\', '/'])
        .find(|part| !part.is_empty())
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
    // could not expose the comparable metadata at all, however, fail safe to a
    // name match instead of classifying a plausible capsule app as unrelated.
    if saved_has_strong_identity && observed_comparable_identity {
        return false;
    }
    current.name.eq_ignore_ascii_case(&saved.name)
}

fn current_executable_name(application: &ApplicationInfo) -> Option<&str> {
    application
        .executable_path
        .as_deref()
        .or_else(|| application.launch.as_ref().map(|launch| launch.target.as_str()))
        .and_then(file_name)
}

fn named_or_executable(application: &ApplicationInfo, values: &[&str]) -> bool {
    values.iter().any(|value| application.name.eq_ignore_ascii_case(value))
        || current_executable_name(application).is_some_and(|name| {
            values.iter().any(|value| name.eq_ignore_ascii_case(value))
        })
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
    named_or_executable(
        application,
        &["WezTerm", "wezterm-gui.exe", "wezterm.exe"],
    )
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

fn is_explorer_shell(application: &ApplicationInfo) -> bool {
    named_or_executable(
        application,
        &["Explorer", "File Explorer", "explorer.exe"],
    )
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
fn request_close(application: &ApplicationInfo) -> Vec<String> {
    let mut errors = Vec::new();
    let mut pids = application.pids.clone();
    pids.sort_unstable();
    pids.dedup();
    if pids.is_empty() {
        pids.push(application.primary_pid);
    }

    // `/F` is deliberately omitted. Replace mode is explicit, but Context
    // Capsule still gives applications a normal close path so unsaved-work
    // dialogs can protect user data instead of being bypassed.
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

#[cfg(windows)]
fn close_unrelated_windows(snapshot: &Value, dry_run: bool) -> ApplicationCleanupReport {
    let mut report = ApplicationCleanupReport::default();
    let saved_desktop = match SavedDesktop::from_capsule(snapshot) {
        Ok(Some(desktop)) => desktop,
        Ok(None) => SavedDesktop {
            status: "available".to_owned(),
            displays: Vec::new(),
            applications: Vec::new(),
        },
        Err(error) => {
            report.failures.push(error);
            return report;
        }
    };
    let current = match crate::desktop::discover() {
        Ok(desktop) => desktop,
        Err(error) => {
            report
                .failures
                .push(format!("could not inspect running applications for replace mode: {error}"));
            return report;
        }
    };
    report.applications_detected = current.applications.len();
    let protected_pids = protected_process_chain();
    let mut candidates = Vec::new();

    for application in current.applications {
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
        if is_explorer_shell(&application) {
            report.applications_protected += 1;
            report.warnings.push(
                "Preserved the Windows Explorer shell in replace mode; Context Capsule never terminates explorer.exe as an application cleanup side effect."
                    .to_owned(),
            );
            continue;
        }
        report.applications_planned_to_close += 1;
        candidates.push(application);
    }

    if dry_run || candidates.is_empty() {
        return report;
    }

    let mut request_errors = HashMap::<String, Vec<String>>::new();
    for application in &candidates {
        report.close_requests_sent += 1;
        let errors = request_close(application);
        if !errors.is_empty() {
            request_errors.insert(application.name.clone(), errors);
        }
    }

    thread::sleep(CLOSE_SETTLE);
    let after = match crate::desktop::discover() {
        Ok(desktop) => desktop,
        Err(error) => {
            report.warnings.push(format!(
                "cleanup requests were sent, but Context Capsule could not verify the resulting desktop state: {error}"
            ));
            return report;
        }
    };

    for application in candidates {
        let still_running = after
            .applications
            .iter()
            .any(|current| current_matches_application(current, &application));
        if still_running {
            report.applications_remaining += 1;
            let details = request_errors
                .remove(&application.name)
                .map(|errors| format!(" ({})", errors.join("; ")))
                .unwrap_or_default();
            report.warnings.push(format!(
                "'{}' is still running after a normal close request{}; it was not force-killed, so an unsaved-work prompt or application shutdown policy can still protect user data.",
                application.name, details
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
}
