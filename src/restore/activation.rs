use super::{SavedApplication, SavedDesktop, model::normalize_windows_path};
use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    mem::{size_of, zeroed},
    path::Path,
    process::{Command, Stdio},
    ptr,
    thread,
    time::{Duration, Instant},
};

type Hwnd = *mut c_void;
type Handle = *mut c_void;
type Bool = i32;
type EnumWindowsProc = Option<unsafe extern "system" fn(Hwnd, isize) -> Bool>;

const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
const MAX_PATH: usize = 260;
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
const ACTIVATION_SPACING: Duration = Duration::from_millis(120);
const VISIBLE_WINDOW_TIMEOUT: Duration = Duration::from_secs(4);
const VISIBLE_WINDOW_POLL: Duration = Duration::from_millis(140);

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

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(callback: EnumWindowsProc, lparam: isize) -> Bool;
    fn IsWindowVisible(hwnd: Hwnd) -> Bool;
    fn GetWindowTextLengthW(hwnd: Hwnd) -> i32;
    fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
    fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> Bool;
    fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> Bool;
    fn OpenProcess(desired_access: u32, inherit_handle: Bool, process_id: u32) -> Handle;
    fn QueryFullProcessImageNameW(
        process: Handle,
        flags: u32,
        executable_name: *mut u16,
        size: *mut u32,
    ) -> Bool;
    fn GetApplicationUserModelId(
        process: Handle,
        application_user_model_id_length: *mut u32,
        application_user_model_id: *mut u16,
    ) -> i32;
    fn CloseHandle(handle: Handle) -> Bool;
}

#[derive(Debug, Clone)]
struct CurrentProcess {
    pid: u32,
    exe_name: String,
    executable_path: Option<String>,
    app_user_model_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivationReport {
    pub candidates: usize,
    pub activated: usize,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

pub fn reactivate_background_only_apps(
    desktop: &SavedDesktop,
    defer_windows_terminal: bool,
) -> ActivationReport {
    let mut report = ActivationReport::default();
    let visible_pids = visible_window_pids();
    let processes = relevant_processes(&desktop.applications);
    let mut candidates = Vec::new();

    for application in &desktop.applications {
        if application.windows.is_empty()
            || is_explorer(application)
            || (defer_windows_terminal && is_windows_terminal(application))
        {
            continue;
        }

        let matching_pids = matching_pids(application, &processes);
        if should_activate(&matching_pids, &visible_pids) {
            candidates.push(application);
        }
    }

    report.candidates = candidates.len();
    let total = candidates.len();
    for (index, application) in candidates.into_iter().enumerate() {
        match activate(application) {
            Ok(()) => {
                report.activated += 1;
                if !wait_for_visible_window(application) {
                    report.warnings.push(format!(
                        "{} was reactivated but no visible window appeared within {} ms; final placement will continue without blocking longer",
                        application.name,
                        VISIBLE_WINDOW_TIMEOUT.as_millis()
                    ));
                }
            }
            Err(error) => report
                .failures
                .push(format!("{}: {error}", application.name)),
        }
        if index + 1 < total {
            thread::sleep(ACTIVATION_SPACING);
        }
    }

    if report.activated > 0 {
        report.warnings.push(format!(
            "desktop restore reactivated {} saved app(s) whose process was still running without a visible window",
            report.activated
        ));
    }
    report
}

fn should_activate(matching_pids: &HashSet<u32>, visible_pids: &HashSet<u32>) -> bool {
    !matching_pids.is_empty() && matching_pids.is_disjoint(visible_pids)
}

fn wait_for_visible_window(application: &SavedApplication) -> bool {
    let deadline = Instant::now() + VISIBLE_WINDOW_TIMEOUT;
    loop {
        let processes = relevant_processes(std::slice::from_ref(application));
        let matching = matching_pids(application, &processes);
        if !matching.is_empty() && !matching.is_disjoint(&visible_window_pids()) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(VISIBLE_WINDOW_POLL);
    }
}

fn matching_pids(application: &SavedApplication, processes: &[CurrentProcess]) -> HashSet<u32> {
    processes
        .iter()
        .filter(|process| process_matches(application, process))
        .map(|process| process.pid)
        .collect()
}

fn activate(application: &SavedApplication) -> Result<(), String> {
    let strategy = application
        .launch
        .as_ref()
        .map(|launch| launch.strategy.as_str())
        .unwrap_or_else(|| {
            if application.app_user_model_id.is_some() {
                "app-user-model-id"
            } else {
                "executable"
            }
        });
    let target = application
        .launch
        .as_ref()
        .map(|launch| launch.target.as_str())
        .or(application
            .app_user_model_id
            .as_deref()
            .filter(|_| strategy == "app-user-model-id"))
        .or(application.executable_path.as_deref())
        .ok_or_else(|| "no launch identity is available".to_owned())?;

    let mut command = match strategy {
        "app-user-model-id" => {
            let mut command = Command::new("explorer.exe");
            command.arg(format!(r"shell:AppsFolder\{target}"));
            command
        }
        "executable" => Command::new(target),
        other => return Err(format!("unsupported launch strategy '{other}'")),
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not reactivate saved application: {error}"))
}

fn visible_window_pids() -> HashSet<u32> {
    let mut pids = HashSet::new();
    let data = (&mut pids as *mut HashSet<u32>) as isize;
    unsafe {
        EnumWindows(Some(enum_visible_window), data);
    }
    pids
}

unsafe extern "system" fn enum_visible_window(hwnd: Hwnd, data: isize) -> Bool {
    if unsafe { IsWindowVisible(hwnd) } == 0 || unsafe { GetWindowTextLengthW(hwnd) } <= 0 {
        return 1;
    }
    let mut pid = 0_u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    if pid != 0 {
        let pids = unsafe { &mut *(data as *mut HashSet<u32>) };
        pids.insert(pid);
    }
    1
}

fn relevant_processes(saved: &[SavedApplication]) -> Vec<CurrentProcess> {
    let candidate_names = candidate_executable_names(saved);
    process_entries()
        .into_iter()
        .filter(|(_, name)| candidate_names.contains(&name.to_ascii_lowercase()))
        .map(|(pid, exe_name)| {
            let (executable_path, app_user_model_id) = process_metadata(pid);
            CurrentProcess {
                pid,
                exe_name,
                executable_path,
                app_user_model_id,
            }
        })
        .collect()
}

fn candidate_executable_names(saved: &[SavedApplication]) -> HashSet<String> {
    let mut names = HashSet::new();
    for application in saved {
        if let Some(name) = saved_executable_name(application) {
            names.insert(name.to_ascii_lowercase());
        }
        let app_name = application.name.trim();
        if !app_name.is_empty() && !app_name.contains(['\\', '/']) {
            let inferred = if app_name.to_ascii_lowercase().ends_with(".exe") {
                app_name.to_owned()
            } else {
                format!("{app_name}.exe")
            };
            names.insert(inferred.to_ascii_lowercase());
        }
    }
    names
}

fn process_entries() -> HashMap<u32, String> {
    let mut result = HashMap::new();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot as isize == -1 {
        return result;
    }

    let mut entry: ProcessEntry32W = unsafe { zeroed() };
    entry.size = size_of::<ProcessEntry32W>() as u32;
    let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while has_entry {
        result.insert(entry.process_id, wide_buffer_to_string(&entry.exe_file));
        entry = unsafe { zeroed() };
        entry.size = size_of::<ProcessEntry32W>() as u32;
        has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe {
        CloseHandle(snapshot);
    }
    result
}

fn process_metadata(pid: u32) -> (Option<String>, Option<String>) {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return (None, None);
    }
    let path = query_process_path(process);
    let aumid = query_app_user_model_id(process);
    unsafe {
        CloseHandle(process);
    }
    (path, aumid)
}

fn query_process_path(process: Handle) -> Option<String> {
    let mut buffer = vec![0_u16; 32_768];
    let mut length = buffer.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) } == 0
        || length == 0
    {
        None
    } else {
        Some(String::from_utf16_lossy(&buffer[..length as usize]))
    }
}

fn query_app_user_model_id(process: Handle) -> Option<String> {
    let mut length = 0_u32;
    let first = unsafe { GetApplicationUserModelId(process, &mut length, ptr::null_mut()) };
    if first != ERROR_INSUFFICIENT_BUFFER || length == 0 {
        return None;
    }
    let mut buffer = vec![0_u16; length as usize];
    if unsafe { GetApplicationUserModelId(process, &mut length, buffer.as_mut_ptr()) } != 0 {
        return None;
    }
    let value = wide_buffer_to_string(&buffer);
    (!value.is_empty()).then_some(value)
}

fn process_matches(application: &SavedApplication, process: &CurrentProcess) -> bool {
    let mut has_strong_identity = false;

    if let Some(saved) = application.app_user_model_id.as_deref() {
        has_strong_identity = true;
        if process
            .app_user_model_id
            .as_deref()
            .is_some_and(|current| current.eq_ignore_ascii_case(saved))
        {
            return true;
        }
    }

    if let Some(saved) = application.executable_path.as_deref() {
        has_strong_identity = true;
        if process.executable_path.as_deref().is_some_and(|current| {
            normalize_windows_path(current) == normalize_windows_path(saved)
        }) {
            return true;
        }
    }

    if let Some(launch) = application.launch.as_ref() {
        match launch.strategy.as_str() {
            "app-user-model-id" => {
                has_strong_identity = true;
                if process
                    .app_user_model_id
                    .as_deref()
                    .is_some_and(|current| current.eq_ignore_ascii_case(&launch.target))
                {
                    return true;
                }
            }
            "executable" => {
                has_strong_identity = true;
                if process.executable_path.as_deref().is_some_and(|current| {
                    normalize_windows_path(current) == normalize_windows_path(&launch.target)
                }) {
                    return true;
                }
            }
            _ => {}
        }
    }

    if has_strong_identity {
        return false;
    }

    process
        .exe_name
        .strip_suffix(".exe")
        .unwrap_or(&process.exe_name)
        .eq_ignore_ascii_case(&application.name)
}

fn saved_executable_name(application: &SavedApplication) -> Option<String> {
    application
        .executable_path
        .as_deref()
        .or_else(|| {
            application
                .launch
                .as_ref()
                .filter(|launch| launch.strategy == "executable")
                .map(|launch| launch.target.as_str())
        })
        .and_then(|value| Path::new(value).file_name())
        .map(|value| value.to_string_lossy().into_owned())
}

fn is_explorer(application: &SavedApplication) -> bool {
    saved_executable_name(application)
        .is_some_and(|name| name.eq_ignore_ascii_case("explorer.exe"))
}

fn is_windows_terminal(application: &SavedApplication) -> bool {
    application
        .app_user_model_id
        .as_deref()
        .is_some_and(|value| value.to_ascii_lowercase().contains("windowsterminal"))
        || saved_executable_name(application).is_some_and(|name| {
            name.eq_ignore_ascii_case("windowsterminal.exe") || name.eq_ignore_ascii_case("wt.exe")
        })
}

fn wide_buffer_to_string(buffer: &[u16]) -> String {
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_process_without_visible_window_requires_activation() {
        let running = HashSet::from([41_u32]);
        assert!(should_activate(&running, &HashSet::new()));
        assert!(!should_activate(&running, &HashSet::from([41_u32])));
        assert!(!should_activate(&HashSet::new(), &HashSet::new()));
    }

    #[test]
    fn candidate_names_include_saved_path_without_scanning_every_process() {
        let application = SavedApplication {
            name: "WhatsApp".to_owned(),
            executable_path: Some(r"C:\Program Files\WindowsApps\WhatsApp.exe".to_owned()),
            app_user_model_id: Some("WhatsApp_123!App".to_owned()),
            file_version: None,
            classification: "user-application".to_owned(),
            launch: None,
            windows: Vec::new(),
            discovered_as_background: false,
        };
        let names = candidate_executable_names(&[application]);
        assert!(names.contains("whatsapp.exe"));
    }

    #[test]
    fn process_matching_falls_back_from_unavailable_aumid_to_exact_path() {
        let application = SavedApplication {
            name: "WhatsApp".to_owned(),
            executable_path: Some(r"C:\Apps\WhatsApp.exe".to_owned()),
            app_user_model_id: Some("WhatsApp_123!App".to_owned()),
            file_version: None,
            classification: "user-application".to_owned(),
            launch: None,
            windows: Vec::new(),
            discovered_as_background: false,
        };
        let process = CurrentProcess {
            pid: 41,
            exe_name: "WhatsApp.exe".to_owned(),
            executable_path: Some(r"c:\apps\whatsapp.exe".to_owned()),
            app_user_model_id: None,
        };
        assert!(process_matches(&application, &process));
    }
}
