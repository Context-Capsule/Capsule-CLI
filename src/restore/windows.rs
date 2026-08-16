use super::{DesktopRestoreReport, model::*};
use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, c_void},
    mem::{size_of, zeroed},
    os::windows::ffi::OsStrExt,
    process::{Command, Stdio},
    ptr,
    thread,
    time::{Duration, Instant},
};

type Hwnd = *mut c_void;
type Handle = *mut c_void;
type Hmonitor = *mut c_void;
type Hdc = *mut c_void;
type Bool = i32;
type Hresult = i32;

type EnumWindowsProc = Option<unsafe extern "system" fn(Hwnd, isize) -> Bool>;
type MonitorEnumProc =
    Option<unsafe extern "system" fn(Hmonitor, Hdc, *mut NativeRect, isize) -> Bool>;

const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
const MAX_PATH: usize = 260;
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
const MONITORINFOF_PRIMARY: u32 = 1;
const MONITOR_DEFAULTTONEAREST: u32 = 2;
const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;

const SW_MINIMIZE: i32 = 6;
const SW_MAXIMIZE: i32 = 3;
const SW_RESTORE: i32 = 9;
const SW_SHOWNOACTIVATE: i32 = 4;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;
const SWP_SHOWWINDOW: u32 = 0x0040;

const LAUNCH_READY_TIMEOUT: Duration = Duration::from_millis(1_500);
const LAUNCH_POLL_INTERVAL: Duration = Duration::from_millis(120);
const WINDOW_SETTLE_TIMEOUT: Duration = Duration::from_secs(8);
const WINDOW_SETTLE_POLL: Duration = Duration::from_millis(250);
const GEOMETRY_TOLERANCE: i32 = 12;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NativeRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl From<NativeRect> for SavedRect {
    fn from(value: NativeRect) -> Self {
        Self {
            left: value.left,
            top: value.top,
            right: value.right,
            bottom: value.bottom,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
struct WindowPlacement {
    length: u32,
    flags: u32,
    show_cmd: u32,
    min_position: Point,
    max_position: Point,
    normal_position: NativeRect,
}

#[repr(C)]
struct MonitorInfoExW {
    size: u32,
    monitor: NativeRect,
    work: NativeRect,
    flags: u32,
    device: [u16; 32],
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
    priority_class_base: i32,
    flags: u32,
    exe_file: [u16; MAX_PATH],
}

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(callback: EnumWindowsProc, lparam: isize) -> Bool;
    fn IsWindowVisible(hwnd: Hwnd) -> Bool;
    fn GetWindowTextLengthW(hwnd: Hwnd) -> i32;
    fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, max_count: i32) -> i32;
    fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
    fn GetWindowRect(hwnd: Hwnd, rect: *mut NativeRect) -> Bool;
    fn GetWindowPlacement(hwnd: Hwnd, placement: *mut WindowPlacement) -> Bool;
    fn IsIconic(hwnd: Hwnd) -> Bool;
    fn IsZoomed(hwnd: Hwnd) -> Bool;
    fn MonitorFromWindow(hwnd: Hwnd, flags: u32) -> Hmonitor;
    fn EnumDisplayMonitors(
        hdc: Hdc,
        clip: *const NativeRect,
        callback: MonitorEnumProc,
        data: isize,
    ) -> Bool;
    fn GetMonitorInfoW(monitor: Hmonitor, info: *mut MonitorInfoExW) -> Bool;
    fn ShowWindow(hwnd: Hwnd, command: i32) -> Bool;
    fn SetWindowPos(
        hwnd: Hwnd,
        insert_after: Hwnd,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        flags: u32,
    ) -> Bool;
    fn SetForegroundWindow(hwnd: Hwnd) -> Bool;
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

#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmGetWindowAttribute(
        hwnd: Hwnd,
        attribute: u32,
        value: *mut c_void,
        value_size: u32,
    ) -> Hresult;
}

#[derive(Debug, Clone)]
struct CurrentProcess {
    pid: u32,
    exe_name: String,
    executable_path: Option<String>,
    app_user_model_id: Option<String>,
}

#[derive(Debug, Clone)]
struct CurrentWindow {
    hwnd: usize,
    pid: u32,
    title: String,
    bounds: SavedRect,
    restore_bounds: Option<SavedRect>,
    minimized: bool,
    maximized: bool,
    display_device: Option<String>,
}

#[derive(Debug, Clone)]
struct CurrentInventory {
    processes: Vec<CurrentProcess>,
    windows: Vec<CurrentWindow>,
    displays: Vec<TargetDisplay>,
}

impl CurrentInventory {
    fn capture() -> Result<Self, String> {
        Ok(Self {
            processes: enumerate_processes(),
            windows: enumerate_windows()?,
            displays: enumerate_displays()?,
        })
    }

    fn matching_pids(&self, app: &SavedApplication) -> HashSet<u32> {
        self.processes
            .iter()
            .filter(|process| process_matches(app, process))
            .map(|process| process.pid)
            .collect()
    }

    fn app_running(&self, app: &SavedApplication) -> bool {
        self.processes
            .iter()
            .any(|process| process_matches(app, process))
    }

    fn app_observable(&self, app: &SavedApplication) -> bool {
        let pids = self.matching_pids(app);
        if pids.is_empty() {
            return false;
        }
        app.discovered_as_background || self.windows.iter().any(|window| pids.contains(&window.pid))
    }

    fn windows_for_app(&self, app: &SavedApplication) -> Vec<&CurrentWindow> {
        let pids = self.matching_pids(app);
        self.windows
            .iter()
            .filter(|window| pids.contains(&window.pid))
            .collect()
    }
}

pub fn restore_desktop(saved: &SavedDesktop, dry_run: bool) -> DesktopRestoreReport {
    let mut report = DesktopRestoreReport {
        applications_total: saved.applications.len(),
        ..DesktopRestoreReport::default()
    };

    let mut inventory = match CurrentInventory::capture() {
        Ok(inventory) => inventory,
        Err(error) => {
            report.failures.push(format!("desktop inventory failed: {error}"));
            return report;
        }
    };

    let mut missing = Vec::new();
    for application in &saved.applications {
        if inventory.app_running(application) {
            report.applications_already_running += 1;
        } else if application.launch.is_some()
            || application.executable_path.is_some()
            || application.app_user_model_id.is_some()
        {
            missing.push(application);
        } else {
            report.applications_unlaunchable += 1;
            report.warnings.push(format!(
                "{} is not running and the snapshot has no launch identity",
                application.name
            ));
        }
    }

    missing.sort_by(|left, right| {
        right
            .discovered_as_background
            .cmp(&left.discovered_as_background)
            .then_with(|| {
                left.foreground_window()
                    .is_some()
                    .cmp(&right.foreground_window().is_some())
            })
            .then_with(|| right.frontmost_z_order().cmp(&left.frontmost_z_order()))
            .then_with(|| left.name.to_ascii_lowercase().cmp(&right.name.to_ascii_lowercase()))
    });

    report.applications_planned_to_launch = missing.len();
    if !dry_run {
        for application in missing {
            match launch_application(application) {
                Ok(()) => {
                    report.applications_launched += 1;
                    inventory = wait_until_observable(application, inventory, &mut report.warnings);
                }
                Err(error) => {
                    report.applications_failed += 1;
                    report.failures.push(format!("{}: {error}", application.name));
                }
            }
        }

        inventory = wait_for_windows(saved, inventory);
    }

    let mut placed_windows: Vec<(usize, usize, bool)> = Vec::new();
    for application in &saved.applications {
        let current_windows = inventory.windows_for_app(application);
        let matches = match_windows(application, &current_windows, &inventory.displays);
        report.windows_total += application.windows.len();
        report.windows_missing += application.windows.len().saturating_sub(matches.len());

        for (saved_window, current_window) in matches {
            let Some(display) = choose_display(saved_window, &inventory.displays) else {
                report.windows_missing += 1;
                report.warnings.push(format!(
                    "{}: no display is available for window '{}'",
                    application.name, saved_window.title
                ));
                continue;
            };
            let target = target_rect(saved_window, display);
            if window_satisfied(current_window, saved_window, display, target) {
                report.windows_already_placed += 1;
            } else if dry_run {
                report.windows_planned_to_move += 1;
            } else {
                match apply_window_state(current_window, saved_window, target) {
                    Ok(()) => report.windows_moved += 1,
                    Err(error) => report.failures.push(format!(
                        "{} / '{}': {error}",
                        application.name, saved_window.title
                    )),
                }
            }
            placed_windows.push((current_window.hwnd, saved_window.z_order, saved_window.is_foreground));
        }
    }

    if !dry_run && !placed_windows.is_empty() {
        restore_z_order_and_foreground(&placed_windows, &mut report.warnings);
    }

    if saved
        .applications
        .iter()
        .flat_map(|application| application.windows.iter())
        .any(|window| window.virtual_desktop_id.is_some())
    {
        report.warnings.push(
            "saved virtual-desktop IDs are preserved in the capsule, but this restore pass does not move windows between Windows virtual desktops"
                .to_owned(),
        );
    }

    report
}

fn wait_until_observable(
    app: &SavedApplication,
    mut inventory: CurrentInventory,
    warnings: &mut Vec<String>,
) -> CurrentInventory {
    let deadline = Instant::now() + LAUNCH_READY_TIMEOUT;
    loop {
        if inventory.app_observable(app) {
            return inventory;
        }
        if Instant::now() >= deadline {
            warnings.push(format!(
                "{} was launched but did not become observable within {} ms; continuing restore while it starts",
                app.name,
                LAUNCH_READY_TIMEOUT.as_millis()
            ));
            return inventory;
        }
        thread::sleep(LAUNCH_POLL_INTERVAL);
        match CurrentInventory::capture() {
            Ok(next) => inventory = next,
            Err(error) => {
                warnings.push(format!(
                    "could not refresh desktop inventory while waiting for {}: {error}",
                    app.name
                ));
                return inventory;
            }
        }
    }
}

fn wait_for_windows(saved: &SavedDesktop, mut inventory: CurrentInventory) -> CurrentInventory {
    let targets: Vec<&SavedApplication> = saved
        .applications
        .iter()
        .filter(|application| !application.windows.is_empty())
        .collect();
    if targets.is_empty()
        || targets
            .iter()
            .all(|application| !inventory.windows_for_app(application).is_empty())
    {
        return inventory;
    }

    let deadline = Instant::now() + WINDOW_SETTLE_TIMEOUT;
    while Instant::now() < deadline {
        thread::sleep(WINDOW_SETTLE_POLL);
        let Ok(next) = CurrentInventory::capture() else {
            break;
        };
        inventory = next;
        if targets
            .iter()
            .all(|application| !inventory.windows_for_app(application).is_empty())
        {
            break;
        }
    }
    inventory
}

fn launch_application(app: &SavedApplication) -> Result<(), String> {
    let launch = app.launch.as_ref();
    let strategy = launch.map(|launch| launch.strategy.as_str()).unwrap_or_else(|| {
        if app.app_user_model_id.is_some() {
            "app-user-model-id"
        } else {
            "executable"
        }
    });
    let target = launch
        .map(|launch| launch.target.as_str())
        .or(app.app_user_model_id.as_deref().filter(|_| strategy == "app-user-model-id"))
        .or(app.executable_path.as_deref())
        .ok_or_else(|| "no launch target is available".to_owned())?;

    let mut command = match strategy {
        "executable" => Command::new(target),
        "app-user-model-id" => {
            let mut command = Command::new("explorer.exe");
            command.arg(format!(r"shell:AppsFolder\{target}"));
            command
        }
        other => return Err(format!("unsupported launch strategy '{other}'")),
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to launch {}: {error}", app.identity_description()))
}

fn process_matches(app: &SavedApplication, process: &CurrentProcess) -> bool {
    let mut has_strong_identity = false;

    if let Some(saved) = app.app_user_model_id.as_deref() {
        has_strong_identity = true;
        if process
            .app_user_model_id
            .as_deref()
            .is_some_and(|current| current.eq_ignore_ascii_case(saved))
        {
            return true;
        }
    }

    if let Some(saved) = app.executable_path.as_deref() {
        has_strong_identity = true;
        if process.executable_path.as_deref().is_some_and(|current| {
            normalize_windows_path(current) == normalize_windows_path(saved)
        }) {
            return true;
        }
    }

    if let Some(launch) = app.launch.as_ref() {
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
        .eq_ignore_ascii_case(&app.name)
}

fn match_windows<'a>(
    app: &'a SavedApplication,
    current: &[&'a CurrentWindow],
    displays: &[TargetDisplay],
) -> Vec<(&'a SavedWindow, &'a CurrentWindow)> {
    let mut saved: Vec<&SavedWindow> = app.windows.iter().collect();
    saved.sort_by_key(|window| window.z_order);
    let mut remaining: Vec<&CurrentWindow> = current.to_vec();
    let mut result = Vec::new();

    for saved_window in saved {
        if remaining.is_empty() {
            break;
        }
        let best = if remaining.len() == 1 {
            Some(0)
        } else {
            remaining
                .iter()
                .enumerate()
                .max_by_key(|(_, candidate)| window_match_score(saved_window, candidate, displays))
                .map(|(index, _)| index)
        };
        if let Some(index) = best {
            result.push((saved_window, remaining.remove(index)));
        }
    }
    result
}

fn window_match_score(
    saved: &SavedWindow,
    current: &CurrentWindow,
    displays: &[TargetDisplay],
) -> i64 {
    let title = title_match_score(&saved.title, &current.title) as i64 * 10_000;
    let geometry = choose_display(saved, displays)
        .map(|display| target_rect(saved, display))
        .map(|target| {
            let distance = (target.left - current.bounds.left).abs() as i64
                + (target.top - current.bounds.top).abs() as i64
                + (target.right - current.bounds.right).abs() as i64
                + (target.bottom - current.bounds.bottom).abs() as i64;
            4_000_i64.saturating_sub(distance.min(4_000))
        })
        .unwrap_or(0);
    title + geometry
}

fn window_satisfied(
    current: &CurrentWindow,
    saved: &SavedWindow,
    display: &TargetDisplay,
    target: SavedRect,
) -> bool {
    match saved.state_spec() {
        WindowStateSpec::Minimized => {
            current.minimized
                && current
                    .restore_bounds
                    .is_some_and(|bounds| rect_close(bounds, target, GEOMETRY_TOLERANCE))
        }
        WindowStateSpec::Maximized => {
            current.maximized
                && current
                    .display_device
                    .as_deref()
                    .is_some_and(|device| device.eq_ignore_ascii_case(&display.device_name))
        }
        WindowStateSpec::Fullscreen => {
            !current.minimized
                && !current.maximized
                && rect_close(current.bounds, display.bounds, GEOMETRY_TOLERANCE)
        }
        WindowStateSpec::Normal | WindowStateSpec::Snapped(_) | WindowStateSpec::Unknown(_) => {
            !current.minimized
                && !current.maximized
                && rect_close(current.bounds, target, GEOMETRY_TOLERANCE)
        }
    }
}

fn apply_window_state(
    current: &CurrentWindow,
    saved: &SavedWindow,
    target: SavedRect,
) -> Result<(), String> {
    let hwnd = current.hwnd as Hwnd;
    if hwnd.is_null() {
        return Err("window handle is unavailable".to_owned());
    }

    if current.minimized || current.maximized {
        unsafe {
            ShowWindow(hwnd, SW_RESTORE);
        }
    } else {
        unsafe {
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }

    let width = target.width().max(1);
    let height = target.height().max(1);
    let moved = unsafe {
        SetWindowPos(
            hwnd,
            ptr::null_mut(),
            target.left,
            target.top,
            width,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    };
    if moved == 0 {
        return Err("SetWindowPos failed".to_owned());
    }

    match saved.state_spec() {
        WindowStateSpec::Minimized => unsafe {
            ShowWindow(hwnd, SW_MINIMIZE);
        },
        WindowStateSpec::Maximized => unsafe {
            ShowWindow(hwnd, SW_MAXIMIZE);
        },
        WindowStateSpec::Normal
        | WindowStateSpec::Fullscreen
        | WindowStateSpec::Snapped(_)
        | WindowStateSpec::Unknown(_) => {}
    }
    Ok(())
}

fn restore_z_order_and_foreground(
    placed: &[(usize, usize, bool)],
    warnings: &mut Vec<String>,
) {
    let mut ordered = placed.to_vec();
    ordered.sort_by_key(|(_, z_order, _)| std::cmp::Reverse(*z_order));
    for (handle, _, _) in &ordered {
        let hwnd = *handle as Hwnd;
        let result = unsafe {
            SetWindowPos(
                hwnd,
                ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        };
        if result == 0 {
            warnings.push("could not fully restore saved window Z-order".to_owned());
            break;
        }
    }

    if let Some((handle, _, _)) = placed.iter().find(|(_, _, foreground)| *foreground) {
        if unsafe { SetForegroundWindow(*handle as Hwnd) } == 0 {
            warnings.push(
                "Windows foreground-lock policy prevented restoring the saved foreground window"
                    .to_owned(),
            );
        }
    }
}

fn enumerate_processes() -> Vec<CurrentProcess> {
    let mut result = Vec::new();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot as isize == -1 {
        return result;
    }

    let mut entry: ProcessEntry32W = unsafe { zeroed() };
    entry.size = size_of::<ProcessEntry32W>() as u32;
    let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while has_entry {
        let pid = entry.process_id;
        let exe_name = wide_buffer_to_string(&entry.exe_file);
        let (executable_path, app_user_model_id) = process_metadata(pid);
        result.push(CurrentProcess {
            pid,
            exe_name,
            executable_path,
            app_user_model_id,
        });
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
    let mut size = buffer.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut size) } == 0
        || size == 0
    {
        None
    } else {
        Some(String::from_utf16_lossy(&buffer[..size as usize]))
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

fn enumerate_windows() -> Result<Vec<CurrentWindow>, String> {
    let mut windows = Vec::new();
    let data = (&mut windows as *mut Vec<CurrentWindow>) as isize;
    if unsafe { EnumWindows(Some(enum_window), data) } == 0 {
        return Err("EnumWindows failed".to_owned());
    }
    Ok(windows)
}

unsafe extern "system" fn enum_window(hwnd: Hwnd, data: isize) -> Bool {
    if unsafe { IsWindowVisible(hwnd) } == 0 {
        return 1;
    }
    let title = window_title(hwnd);
    if title.is_empty() {
        return 1;
    }
    let mut pid = 0_u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    if pid == 0 {
        return 1;
    }
    let Some(bounds) = window_bounds(hwnd) else {
        return 1;
    };
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let display_device = monitor_info(monitor).map(|display| display.device_name);
    let windows = unsafe { &mut *(data as *mut Vec<CurrentWindow>) };
    windows.push(CurrentWindow {
        hwnd: hwnd as usize,
        pid,
        title,
        bounds,
        restore_bounds: window_restore_bounds(hwnd),
        minimized: unsafe { IsIconic(hwnd) != 0 },
        maximized: unsafe { IsZoomed(hwnd) != 0 },
        display_device,
    });
    1
}

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

fn window_bounds(hwnd: Hwnd) -> Option<SavedRect> {
    let mut rect = NativeRect::default();
    let dwm = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut rect as *mut NativeRect).cast(),
            size_of::<NativeRect>() as u32,
        )
    };
    if dwm >= 0 {
        return Some(rect.into());
    }
    if unsafe { GetWindowRect(hwnd, &mut rect) } != 0 {
        Some(rect.into())
    } else {
        None
    }
}

fn window_restore_bounds(hwnd: Hwnd) -> Option<SavedRect> {
    let mut placement: WindowPlacement = unsafe { zeroed() };
    placement.length = size_of::<WindowPlacement>() as u32;
    if unsafe { GetWindowPlacement(hwnd, &mut placement) } == 0 {
        return None;
    }
    let rect: SavedRect = placement.normal_position.into();
    (rect.width() > 0 && rect.height() > 0).then_some(rect)
}

fn enumerate_displays() -> Result<Vec<TargetDisplay>, String> {
    let mut displays = Vec::new();
    let data = (&mut displays as *mut Vec<TargetDisplay>) as isize;
    if unsafe { EnumDisplayMonitors(ptr::null_mut(), ptr::null(), Some(enum_monitor), data) } == 0 {
        return Err("EnumDisplayMonitors failed".to_owned());
    }
    annotate_display_relations(&mut displays);
    displays.sort_by(|left, right| {
        right
            .is_primary
            .cmp(&left.is_primary)
            .then_with(|| left.bounds.left.cmp(&right.bounds.left))
            .then_with(|| left.bounds.top.cmp(&right.bounds.top))
    });
    Ok(displays)
}

unsafe extern "system" fn enum_monitor(
    monitor: Hmonitor,
    _hdc: Hdc,
    _rect: *mut NativeRect,
    data: isize,
) -> Bool {
    if let Some(info) = monitor_info(monitor) {
        let displays = unsafe { &mut *(data as *mut Vec<TargetDisplay>) };
        displays.push(info);
    }
    1
}

fn monitor_info(monitor: Hmonitor) -> Option<TargetDisplay> {
    if monitor.is_null() {
        return None;
    }
    let mut info = MonitorInfoExW {
        size: size_of::<MonitorInfoExW>() as u32,
        monitor: NativeRect::default(),
        work: NativeRect::default(),
        flags: 0,
        device: [0; 32],
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }
    Some(TargetDisplay {
        device_name: wide_buffer_to_string(&info.device),
        bounds: info.monitor.into(),
        work_area: info.work.into(),
        is_primary: info.flags & MONITORINFOF_PRIMARY != 0,
        relation_to_primary: String::new(),
    })
}

fn annotate_display_relations(displays: &mut [TargetDisplay]) {
    let Some(primary) = displays
        .iter()
        .find(|display| display.is_primary)
        .map(|display| display.bounds)
        .or_else(|| displays.first().map(|display| display.bounds))
    else {
        return;
    };
    for display in displays {
        display.relation_to_primary = display_relation(display.bounds, primary, display.is_primary);
    }
}

fn display_relation(display: SavedRect, primary: SavedRect, is_primary: bool) -> String {
    if is_primary {
        return "primary".to_owned();
    }
    let (x, y) = display.center();
    let (primary_x, primary_y) = primary.center();
    let dx = x - primary_x;
    let dy = y - primary_y;
    let horizontal = if dx > 0.0 { "right" } else { "left" };
    let vertical = if dy > 0.0 { "below" } else { "above" };
    if dx.abs() > dy.abs() * 1.5 {
        format!("{horizontal}-of-primary")
    } else if dy.abs() > dx.abs() * 1.5 {
        format!("{vertical}-primary")
    } else {
        format!("{vertical}-{horizontal}-of-primary")
    }
}

fn wide_buffer_to_string(buffer: &[u16]) -> String {
    let length = buffer.iter().position(|character| *character == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

#[allow(dead_code)]
fn to_wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(path: Option<&str>, aumid: Option<&str>, name: &str) -> SavedApplication {
        SavedApplication {
            name: name.to_owned(),
            executable_path: path.map(str::to_owned),
            app_user_model_id: aumid.map(str::to_owned),
            file_version: None,
            classification: "user-application".to_owned(),
            launch: None,
            windows: Vec::new(),
            discovered_as_background: false,
        }
    }

    #[test]
    fn process_identity_prefers_aumid_or_executable_path() {
        let process = CurrentProcess {
            pid: 1,
            exe_name: "Code.exe".to_owned(),
            executable_path: Some(r"C:\Apps\Code.exe".to_owned()),
            app_user_model_id: Some("Microsoft.VisualStudioCode".to_owned()),
        };
        assert!(process_matches(
            &app(Some(r"c:\apps\CODE.exe"), None, "unrelated"),
            &process
        ));
        assert!(process_matches(
            &app(None, Some("microsoft.visualstudiocode"), "unrelated"),
            &process
        ));
        assert!(!process_matches(
            &app(Some(r"C:\Apps\Other.exe"), None, "Code"),
            &process
        ));
    }

    #[test]
    fn launch_order_keeps_saved_foreground_application_until_last() {
        let mut foreground = app(Some(r"C:\foreground.exe"), None, "Foreground");
        foreground.windows.push(SavedWindow {
            title: "Foreground".to_owned(),
            bounds: SavedRect { left: 0, top: 0, right: 100, bottom: 100 },
            restore_bounds: None,
            normalized_bounds: None,
            state: "normal".to_owned(),
            display_device: "DISPLAY1".to_owned(),
            display_relation: "primary".to_owned(),
            display_scale_percent: 100,
            is_foreground: true,
            z_order: 0,
            virtual_desktop_id: None,
            is_on_current_virtual_desktop: None,
            taskbar_candidate: true,
        });
        let mut background = app(Some(r"C:\background.exe"), None, "Background");
        background.discovered_as_background = true;
        let mut regular = app(Some(r"C:\regular.exe"), None, "Regular");
        regular.windows = foreground.windows.clone();
        regular.windows[0].is_foreground = false;
        regular.windows[0].z_order = 5;

        let mut items = [&foreground, &background, &regular];
        items.sort_by(|left, right| {
            right
                .discovered_as_background
                .cmp(&left.discovered_as_background)
                .then_with(|| left.foreground_window().is_some().cmp(&right.foreground_window().is_some()))
                .then_with(|| right.frontmost_z_order().cmp(&left.frontmost_z_order()))
        });
        assert_eq!(items[0].name, "Background");
        assert_eq!(items[2].name, "Foreground");
    }
}
