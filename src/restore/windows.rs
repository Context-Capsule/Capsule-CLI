use super::{DesktopRestoreReport, model::*};
use std::{
    collections::HashSet,
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

const LAUNCH_GATE_TIMEOUT: Duration = Duration::from_millis(1_500);
const LAUNCH_POLL_INTERVAL: Duration = Duration::from_millis(120);
const SETTLE_MAXIMUM: Duration = Duration::from_secs(8);
const SETTLE_STABLE_FOR: Duration = Duration::from_millis(900);
const SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(180);
const GEOMETRY_TOLERANCE: i32 = 14;
const PLACEMENT_RETRIES: usize = 5;
const PLACEMENT_SETTLE_BASE_MS: u64 = 120;

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
    fn GetForegroundWindow() -> Hwnd;
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
struct ProcessEntry {
    pid: u32,
    exe_name: String,
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
    minimized: bool,
    maximized: bool,
    display_device: Option<String>,
    z_order: usize,
    is_foreground: bool,
}

#[derive(Debug, Clone)]
struct CurrentInventory {
    processes: Vec<CurrentProcess>,
    windows: Vec<CurrentWindow>,
    displays: Vec<TargetDisplay>,
}

impl CurrentInventory {
    fn initial(saved: &SavedDesktop) -> Result<Self, String> {
        let displays = enumerate_displays()?;
        let mut inventory = Self {
            processes: Vec::new(),
            windows: Vec::new(),
            displays,
        };
        inventory.refresh_dynamic(&saved.applications)?;
        Ok(inventory)
    }

    fn refresh_dynamic(&mut self, saved: &[SavedApplication]) -> Result<(), String> {
        self.windows = enumerate_windows()?;
        let visible_pids = self
            .windows
            .iter()
            .map(|window| window.pid)
            .collect::<HashSet<_>>();
        self.processes = enumerate_relevant_processes(saved, &visible_pids);
        Ok(())
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

    fn windows_for_app(&self, app: &SavedApplication) -> Vec<&CurrentWindow> {
        let pids = self.matching_pids(app);
        self.windows
            .iter()
            .filter(|window| pids.contains(&window.pid))
            .collect()
    }

    fn app_ready_for_next_launch(&self, app: &SavedApplication) -> bool {
        if !self.app_running(app) {
            return false;
        }
        app.discovered_as_background
            || app.windows.is_empty()
            || !self.windows_for_app(app).is_empty()
    }
}

pub fn restore_desktop(saved: &SavedDesktop, dry_run: bool) -> DesktopRestoreReport {
    let mut report = DesktopRestoreReport {
        applications_total: saved.applications.len(),
        ..DesktopRestoreReport::default()
    };

    let mut inventory = match CurrentInventory::initial(saved) {
        Ok(inventory) => inventory,
        Err(error) => {
            report.failures.push(format!("desktop inventory failed: {error}"));
            return report;
        }
    };

    let mut missing = Vec::new();
    for app in &saved.applications {
        if inventory.app_running(app) {
            report.applications_already_running += 1;
        } else if explorer_folder_app(app) {
            report.applications_unlaunchable += 1;
            report.warnings.push(format!(
                "{} is an Explorer folder window, but its folder path was not captured; refusing to open an arbitrary Explorer Home window",
                app.name
            ));
        } else if has_launch_identity(app) {
            missing.push(app);
        } else {
            report.applications_unlaunchable += 1;
            report.warnings.push(format!(
                "{} is not running and the snapshot has no launch identity",
                app.name
            ));
        }
    }

    sort_launch_queue(&mut missing);
    report.applications_planned_to_launch = missing.len();

    if !dry_run {
        for app in missing {
            match launch_application(app) {
                Ok(()) => {
                    report.applications_launched += 1;
                    wait_launch_gate(app, saved, &mut inventory, &mut report.warnings);
                }
                Err(error) => {
                    report.applications_failed += 1;
                    report.failures.push(format!("{}: {error}", app.name));
                }
            }
        }
        settle_new_windows(saved, &mut inventory, &mut report.warnings);
    }

    let mut matched_for_order = Vec::new();
    for app in &saved.applications {
        let current = inventory.windows_for_app(app);
        let matches = match_windows(app, &current, &inventory.displays);
        report.windows_total += app.windows.len();
        report.windows_missing += app.windows.len().saturating_sub(matches.len());

        for (saved_window, current_window) in matches {
            let Some(display) = choose_display(saved_window, &inventory.displays) else {
                report.windows_missing += 1;
                report.warnings.push(format!(
                    "{}: no current display is available for '{}'",
                    app.name, saved_window.title
                ));
                continue;
            };
            let target = target_rect(saved_window, display);
            if window_satisfied(current_window, saved_window, display, target) {
                report.windows_already_placed += 1;
            } else if dry_run {
                report.windows_planned_to_move += 1;
            } else {
                match apply_window_state(current_window, saved_window, display, target) {
                    Ok(()) => report.windows_moved += 1,
                    Err(error) => report.failures.push(format!(
                        "{} / '{}': {error}",
                        app.name, saved_window.title
                    )),
                }
            }
            matched_for_order.push((saved_window, current_window));
        }
    }

    if !dry_run && !matched_for_order.is_empty() {
        reconcile_order_and_foreground(&matched_for_order, &mut report.warnings);
    }

    if saved
        .applications
        .iter()
        .flat_map(|app| app.windows.iter())
        .any(|window| window.virtual_desktop_id.is_some())
    {
        report.warnings.push(
            "virtual-desktop IDs are preserved in the capsule, but moving windows between Windows virtual desktops is not enabled in this restore pass"
                .to_owned(),
        );
    }

    report
}

fn has_launch_identity(app: &SavedApplication) -> bool {
    app.launch.is_some() || app.executable_path.is_some() || app.app_user_model_id.is_some()
}

fn explorer_folder_app(app: &SavedApplication) -> bool {
    app.windows.len() > 0
        && app
            .executable_path
            .as_deref()
            .and_then(|path| Path::new(path).file_name())
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("explorer.exe"))
}

fn sort_launch_queue(apps: &mut Vec<&SavedApplication>) {
    apps.sort_by(|left, right| {
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
}

fn wait_launch_gate(
    app: &SavedApplication,
    saved: &SavedDesktop,
    inventory: &mut CurrentInventory,
    warnings: &mut Vec<String>,
) {
    let deadline = Instant::now() + LAUNCH_GATE_TIMEOUT;
    loop {
        if inventory.app_ready_for_next_launch(app) {
            return;
        }
        if Instant::now() >= deadline {
            warnings.push(format!(
                "{} is still starting after {} ms; continuing with the next app so startup can overlap",
                app.name,
                LAUNCH_GATE_TIMEOUT.as_millis()
            ));
            return;
        }
        thread::sleep(LAUNCH_POLL_INTERVAL);
        if let Err(error) = inventory.refresh_dynamic(&saved.applications) {
            warnings.push(format!(
                "could not refresh desktop state while waiting for {}: {error}",
                app.name
            ));
            return;
        }
    }
}

fn settle_new_windows(
    saved: &SavedDesktop,
    inventory: &mut CurrentInventory,
    warnings: &mut Vec<String>,
) {
    if saved.applications.iter().all(|app| app.windows.is_empty()) {
        return;
    }

    let start = Instant::now();
    let mut stable_since = Instant::now();
    let mut best_observed = observed_saved_window_count(saved, inventory);

    while start.elapsed() < SETTLE_MAXIMUM {
        if best_observed >= saved_window_count(saved) {
            return;
        }
        if stable_since.elapsed() >= SETTLE_STABLE_FOR {
            return;
        }

        thread::sleep(SETTLE_POLL_INTERVAL);
        if let Err(error) = inventory.refresh_dynamic(&saved.applications) {
            warnings.push(format!("could not refresh desktop state during settle: {error}"));
            return;
        }
        let observed = observed_saved_window_count(saved, inventory);
        if observed > best_observed {
            best_observed = observed;
            stable_since = Instant::now();
        }
    }
}

fn saved_window_count(saved: &SavedDesktop) -> usize {
    saved.applications.iter().map(|app| app.windows.len()).sum()
}

fn observed_saved_window_count(saved: &SavedDesktop, inventory: &CurrentInventory) -> usize {
    saved
        .applications
        .iter()
        .map(|app| inventory.windows_for_app(app).len().min(app.windows.len()))
        .sum()
}

fn launch_application(app: &SavedApplication) -> Result<(), String> {
    let strategy = app
        .launch
        .as_ref()
        .map(|launch| launch.strategy.as_str())
        .unwrap_or_else(|| {
            if app.app_user_model_id.is_some() {
                "app-user-model-id"
            } else {
                "executable"
            }
        });
    let target = app
        .launch
        .as_ref()
        .map(|launch| launch.target.as_str())
        .or(app
            .app_user_model_id
            .as_deref()
            .filter(|_| strategy == "app-user-model-id"))
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

fn enumerate_relevant_processes(
    saved: &[SavedApplication],
    visible_pids: &HashSet<u32>,
) -> Vec<CurrentProcess> {
    let entries = process_entries();
    let candidate_names = saved_candidate_executable_names(saved);
    let needs_aumid_from_visible = saved.iter().any(|app| {
        app.app_user_model_id.is_some()
            || app
                .launch
                .as_ref()
                .is_some_and(|launch| launch.strategy == "app-user-model-id")
    });

    entries
        .into_iter()
        .map(|entry| {
            let should_query = candidate_names.contains(&entry.exe_name.to_ascii_lowercase())
                || (needs_aumid_from_visible && visible_pids.contains(&entry.pid));
            let (executable_path, app_user_model_id) = if should_query {
                process_metadata(entry.pid)
            } else {
                (None, None)
            };
            CurrentProcess {
                pid: entry.pid,
                exe_name: entry.exe_name,
                executable_path,
                app_user_model_id,
            }
        })
        .collect()
}

fn saved_candidate_executable_names(saved: &[SavedApplication]) -> HashSet<String> {
    let mut names = HashSet::new();
    for app in saved {
        if let Some(path) = app.executable_path.as_deref() {
            if let Some(name) = file_name(path) {
                names.insert(name.to_ascii_lowercase());
            }
        }
        if let Some(launch) = app
            .launch
            .as_ref()
            .filter(|launch| launch.strategy == "executable")
        {
            if let Some(name) = file_name(&launch.target) {
                names.insert(name.to_ascii_lowercase());
            }
        }
        names.insert(format!("{}.exe", app.name).to_ascii_lowercase());
    }
    names
}

fn file_name(path: &str) -> Option<&str> {
    Path::new(path).file_name()?.to_str()
}

fn process_matches(app: &SavedApplication, process: &CurrentProcess) -> bool {
    let mut strong_identity = false;

    if let Some(saved) = app.app_user_model_id.as_deref() {
        strong_identity = true;
        if process
            .app_user_model_id
            .as_deref()
            .is_some_and(|current| current.eq_ignore_ascii_case(saved))
        {
            return true;
        }
    }

    if let Some(saved) = app.executable_path.as_deref() {
        strong_identity = true;
        if process.executable_path.as_deref().is_some_and(|current| {
            normalize_windows_path(current) == normalize_windows_path(saved)
        }) {
            return true;
        }
    }

    if let Some(launch) = app.launch.as_ref() {
        match launch.strategy.as_str() {
            "app-user-model-id" => {
                strong_identity = true;
                if process
                    .app_user_model_id
                    .as_deref()
                    .is_some_and(|current| current.eq_ignore_ascii_case(&launch.target))
                {
                    return true;
                }
            }
            "executable" => {
                strong_identity = true;
                if process.executable_path.as_deref().is_some_and(|current| {
                    normalize_windows_path(current) == normalize_windows_path(&launch.target)
                }) {
                    return true;
                }
            }
            _ => {}
        }
    }

    if strong_identity {
        return false;
    }

    process
        .exe_name
        .strip_suffix(".exe")
        .unwrap_or(&process.exe_name)
        .eq_ignore_ascii_case(&app.name)
}

fn match_windows<'saved, 'current>(
    app: &'saved SavedApplication,
    current: &[&'current CurrentWindow],
    displays: &[TargetDisplay],
) -> Vec<(&'saved SavedWindow, &'current CurrentWindow)> {
    let mut saved_windows = app.windows.iter().collect::<Vec<_>>();
    saved_windows.sort_by_key(|window| window.z_order);
    let mut remaining = current.to_vec();
    let mut result = Vec::new();

    for saved_window in saved_windows {
        if remaining.is_empty() {
            break;
        }
        let best = remaining
            .iter()
            .enumerate()
            .max_by_key(|(_, candidate)| window_match_score(saved_window, candidate, displays))
            .map(|(index, _)| index);
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
                    .display_device
                    .as_deref()
                    .is_some_and(|device| device.eq_ignore_ascii_case(&display.device_name))
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
    display: &TargetDisplay,
    target: SavedRect,
) -> Result<(), String> {
    let hwnd = current.hwnd as Hwnd;
    if hwnd.is_null() {
        return Err("window handle is unavailable".to_owned());
    }

    let mut last_observed: Option<CurrentWindow> = None;
    for attempt in 0..PLACEMENT_RETRIES {
        let was_non_normal = unsafe { IsIconic(hwnd) != 0 || IsZoomed(hwnd) != 0 };
        if was_non_normal {
            unsafe {
                ShowWindow(hwnd, SW_RESTORE);
            }
            thread::sleep(Duration::from_millis(40));
        } else {
            unsafe {
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
        }

        let outer = frame_adjusted_outer_rect(hwnd, target);
        if unsafe {
            SetWindowPos(
                hwnd,
                ptr::null_mut(),
                outer.left,
                outer.top,
                outer.width().max(1),
                outer.height().max(1),
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        } == 0
        {
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

        thread::sleep(Duration::from_millis(
            PLACEMENT_SETTLE_BASE_MS * (attempt as u64 + 1),
        ));

        if matches!(saved.state_spec(), WindowStateSpec::Minimized)
            && unsafe { IsIconic(hwnd) != 0 }
        {
            return Ok(());
        }

        if let Some(observed) = observe_window(hwnd) {
            if window_satisfied(&observed, saved, display, target) {
                return Ok(());
            }
            last_observed = Some(observed);
        }
    }

    let observed = last_observed
        .as_ref()
        .map(|window| {
            format!(
                "left={} top={} right={} bottom={} minimized={} maximized={} display={}",
                window.bounds.left,
                window.bounds.top,
                window.bounds.right,
                window.bounds.bottom,
                window.minimized,
                window.maximized,
                window.display_device.as_deref().unwrap_or("unknown")
            )
        })
        .unwrap_or_else(|| "window became unavailable".to_owned());
    Err(format!(
        "window placement did not converge after {PLACEMENT_RETRIES} attempts; desired left={} top={} right={} bottom={} state={}; observed {observed}",
        target.left, target.top, target.right, target.bottom, saved.state
    ))
}

fn observe_window(hwnd: Hwnd) -> Option<CurrentWindow> {
    if hwnd.is_null() {
        return None;
    }
    let bounds = window_bounds(hwnd)?;
    let mut pid = 0_u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let display_device = monitor_info(monitor).map(|display| display.device_name);
    Some(CurrentWindow {
        hwnd: hwnd as usize,
        pid,
        title: window_title(hwnd),
        bounds,
        minimized: unsafe { IsIconic(hwnd) != 0 },
        maximized: unsafe { IsZoomed(hwnd) != 0 },
        display_device,
        z_order: 0,
        is_foreground: hwnd as usize == unsafe { GetForegroundWindow() } as usize,
    })
}

fn frame_adjusted_outer_rect(hwnd: Hwnd, desired_frame: SavedRect) -> SavedRect {
    let mut outer = NativeRect::default();
    let mut frame = NativeRect::default();
    if unsafe { GetWindowRect(hwnd, &mut outer) } == 0
        || unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                (&mut frame as *mut NativeRect).cast(),
                size_of::<NativeRect>() as u32,
            )
        } < 0
    {
        return desired_frame;
    }

    let left_inset = frame.left.saturating_sub(outer.left);
    let top_inset = frame.top.saturating_sub(outer.top);
    let right_inset = outer.right.saturating_sub(frame.right);
    let bottom_inset = outer.bottom.saturating_sub(frame.bottom);
    SavedRect {
        left: desired_frame.left.saturating_sub(left_inset),
        top: desired_frame.top.saturating_sub(top_inset),
        right: desired_frame.right.saturating_add(right_inset),
        bottom: desired_frame.bottom.saturating_add(bottom_inset),
    }
}

fn reconcile_order_and_foreground(
    matches: &[(&SavedWindow, &CurrentWindow)],
    warnings: &mut Vec<String>,
) {
    let mut desired = matches.to_vec();
    desired.sort_by_key(|(saved, _)| saved.z_order);

    let relative_order_is_correct = desired
        .windows(2)
        .all(|pair| pair[0].1.z_order <= pair[1].1.z_order);
    if !relative_order_is_correct {
        for (_, current) in desired.iter().rev() {
            if unsafe {
                SetWindowPos(
                    current.hwnd as Hwnd,
                    ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                )
            } == 0
            {
                warnings.push("could not fully restore relative window Z-order".to_owned());
                break;
            }
        }
    }

    if let Some((_, current)) = desired.iter().find(|(saved, _)| saved.is_foreground) {
        if !current.is_foreground && unsafe { SetForegroundWindow(current.hwnd as Hwnd) } == 0 {
            warnings.push(
                "Windows foreground-lock policy prevented restoring the saved foreground window"
                    .to_owned(),
            );
        }
    }
}

fn process_entries() -> Vec<ProcessEntry> {
    let mut result = Vec::new();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot as isize == -1 {
        return result;
    }

    let mut entry: ProcessEntry32W = unsafe { zeroed() };
    entry.size = size_of::<ProcessEntry32W>() as u32;
    let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while has_entry {
        result.push(ProcessEntry {
            pid: entry.process_id,
            exe_name: wide_buffer_to_string(&entry.exe_file),
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

fn enumerate_windows() -> Result<Vec<CurrentWindow>, String> {
    let foreground = unsafe { GetForegroundWindow() } as usize;
    let mut context = WindowEnumeration {
        windows: Vec::new(),
        foreground,
    };
    let data = (&mut context as *mut WindowEnumeration) as isize;
    if unsafe { EnumWindows(Some(enum_window), data) } == 0 {
        return Err("EnumWindows failed".to_owned());
    }
    Ok(context.windows)
}

struct WindowEnumeration {
    windows: Vec<CurrentWindow>,
    foreground: usize,
}

unsafe extern "system" fn enum_window(hwnd: Hwnd, data: isize) -> Bool {
    if unsafe { IsWindowVisible(hwnd) } == 0 {
        return 1;
    }
    let title = window_title(hwnd);
    if title.is_empty() || title.eq_ignore_ascii_case("Program Manager") {
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
    let context = unsafe { &mut *(data as *mut WindowEnumeration) };
    let z_order = context.windows.len();
    context.windows.push(CurrentWindow {
        hwnd: hwnd as usize,
        pid,
        title,
        bounds,
        minimized: unsafe { IsIconic(hwnd) != 0 },
        maximized: unsafe { IsZoomed(hwnd) != 0 },
        display_device,
        z_order,
        is_foreground: hwnd as usize == context.foreground,
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
    if unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut rect as *mut NativeRect).cast(),
            size_of::<NativeRect>() as u32,
        )
    } >= 0
    {
        return Some(rect.into());
    }
    if unsafe { GetWindowRect(hwnd, &mut rect) } != 0 {
        Some(rect.into())
    } else {
        None
    }
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
    let length = buffer
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
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

    fn saved_window(title: &str, z_order: usize, foreground: bool) -> SavedWindow {
        SavedWindow {
            title: title.to_owned(),
            bounds: SavedRect {
                left: 0,
                top: 0,
                right: 800,
                bottom: 600,
            },
            restore_bounds: None,
            normalized_bounds: None,
            state: "normal".to_owned(),
            display_device: "DISPLAY1".to_owned(),
            display_relation: "primary".to_owned(),
            display_scale_percent: 100,
            is_foreground: foreground,
            z_order,
            virtual_desktop_id: None,
            is_on_current_virtual_desktop: None,
            taskbar_candidate: true,
        }
    }

    #[test]
    fn process_identity_prefers_strong_aumid_or_path() {
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
    fn launch_queue_starts_background_first_and_saved_foreground_last() {
        let mut foreground = app(Some(r"C:\foreground.exe"), None, "Foreground");
        foreground.windows.push(saved_window("Foreground", 0, true));

        let mut background = app(Some(r"C:\background.exe"), None, "Background");
        background.discovered_as_background = true;

        let mut regular = app(Some(r"C:\regular.exe"), None, "Regular");
        regular.windows.push(saved_window("Regular", 5, false));

        let mut queue = vec![&foreground, &background, &regular];
        sort_launch_queue(&mut queue);
        assert_eq!(queue[0].name, "Background");
        assert_eq!(queue[1].name, "Regular");
        assert_eq!(queue[2].name, "Foreground");
    }

    #[test]
    fn explorer_folder_is_not_launched_without_a_folder_path() {
        let mut explorer = app(Some(r"C:\Windows\explorer.exe"), None, "Explorer");
        explorer.windows.push(saved_window("Downloads", 0, true));
        assert!(explorer_folder_app(&explorer));
    }

    #[test]
    fn matching_windows_prefers_saved_titles_over_geometry() {
        let mut application = app(Some(r"C:\app.exe"), None, "App");
        application.windows = vec![
            saved_window("Left document", 0, true),
            saved_window("Right document", 1, false),
        ];
        let current_left = CurrentWindow {
            hwnd: 1,
            pid: 1,
            title: "Left document".to_owned(),
            bounds: SavedRect {
                left: 900,
                top: 0,
                right: 1700,
                bottom: 600,
            },
            minimized: false,
            maximized: false,
            display_device: Some("DISPLAY1".to_owned()),
            z_order: 1,
            is_foreground: false,
        };
        let current_right = CurrentWindow {
            hwnd: 2,
            pid: 1,
            title: "Right document".to_owned(),
            bounds: SavedRect {
                left: 0,
                top: 0,
                right: 800,
                bottom: 600,
            },
            minimized: false,
            maximized: false,
            display_device: Some("DISPLAY1".to_owned()),
            z_order: 0,
            is_foreground: true,
        };
        let display = TargetDisplay {
            device_name: "DISPLAY1".to_owned(),
            bounds: SavedRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            work_area: SavedRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            },
            is_primary: true,
            relation_to_primary: "primary".to_owned(),
        };
        let matches = match_windows(
            &application,
            &[&current_right, &current_left],
            &[display],
        );
        assert_eq!(matches[0].1.hwnd, 1);
        assert_eq!(matches[1].1.hwnd, 2);
    }
}
