use super::{
    classify::{classify_candidate, detect_snap, display_relation, is_known_background_app},
    model::{
        ApplicationClassification, ApplicationInfo, DesktopSnapshot, DisplayInfo, IgnoredCandidate,
        LaunchSpec, LaunchStrategy, NormalizedRect, Rect, WindowInfo, WindowState,
    },
};
use std::{
    collections::{BTreeMap, HashMap},
    ffi::{c_void, OsStr},
    mem::{size_of, zeroed},
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr,
};

type Hwnd = *mut c_void;
type Handle = *mut c_void;
type Hmonitor = *mut c_void;
type Hdc = *mut c_void;
type Hresult = i32;
type Bool = i32;

type EnumWindowsProc = Option<unsafe extern "system" fn(Hwnd, isize) -> Bool>;
type MonitorEnumProc = Option<unsafe extern "system" fn(Hmonitor, Hdc, *mut NativeRect, isize) -> Bool>;

const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
const MAX_PATH: usize = 260;
const GWL_EXSTYLE: i32 = -20;
const GW_OWNER: u32 = 4;
const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
const WS_EX_APPWINDOW: isize = 0x0004_0000;
const MONITOR_DEFAULTTONEAREST: u32 = 2;
const MONITORINFOF_PRIMARY: u32 = 1;
const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;
const DWMWA_CLOAKED: u32 = 14;
const MDT_EFFECTIVE_DPI: i32 = 0;
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
const CLSCTX_ALL: u32 = 0x17;
const COINIT_APARTMENTTHREADED: u32 = 0x2;
const RPC_E_CHANGED_MODE: Hresult = 0x8001_0106_u32 as i32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NativeRect { left: i32, top: i32, right: i32, bottom: i32 }

impl From<NativeRect> for Rect {
    fn from(value: NativeRect) -> Self {
        Self { left: value.left, top: value.top, right: value.right, bottom: value.bottom }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point { x: i32, y: i32 }

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

#[repr(C)]
struct VsFixedFileInfo {
    signature: u32,
    struct_version: u32,
    file_version_ms: u32,
    file_version_ls: u32,
    product_version_ms: u32,
    product_version_ls: u32,
    file_flags_mask: u32,
    file_flags: u32,
    file_os: u32,
    file_type: u32,
    file_subtype: u32,
    file_date_ms: u32,
    file_date_ls: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid { data1: u32, data2: u16, data3: u16, data4: [u8; 8] }

#[repr(C)]
struct VirtualDesktopManager { vtable: *const VirtualDesktopManagerVtable }

#[repr(C)]
struct VirtualDesktopManagerVtable {
    query_interface: unsafe extern "system" fn(*mut VirtualDesktopManager, *const Guid, *mut *mut c_void) -> Hresult,
    add_ref: unsafe extern "system" fn(*mut VirtualDesktopManager) -> u32,
    release: unsafe extern "system" fn(*mut VirtualDesktopManager) -> u32,
    is_window_on_current_virtual_desktop: unsafe extern "system" fn(*mut VirtualDesktopManager, Hwnd, *mut Bool) -> Hresult,
    get_window_desktop_id: unsafe extern "system" fn(*mut VirtualDesktopManager, Hwnd, *mut Guid) -> Hresult,
    move_window_to_desktop: unsafe extern "system" fn(*mut VirtualDesktopManager, Hwnd, *const Guid) -> Hresult,
}

const CLSID_VIRTUAL_DESKTOP_MANAGER: Guid = Guid {
    data1: 0xaa50_9086,
    data2: 0x5ca9,
    data3: 0x4c25,
    data4: [0x8f, 0x95, 0x58, 0x9d, 0x3c, 0x07, 0xb4, 0x8a],
};
const IID_VIRTUAL_DESKTOP_MANAGER: Guid = Guid {
    data1: 0xa5cd_92ff,
    data2: 0x29be,
    data3: 0x454c,
    data4: [0x8d, 0x04, 0xd8, 0x28, 0x79, 0xfb, 0x3f, 0x1b],
};

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
    fn EnumDisplayMonitors(hdc: Hdc, clip: *const NativeRect, callback: MonitorEnumProc, data: isize) -> Bool;
    fn GetMonitorInfoW(monitor: Hmonitor, info: *mut MonitorInfoExW) -> Bool;
    fn GetForegroundWindow() -> Hwnd;
    fn GetWindow(hwnd: Hwnd, command: u32) -> Hwnd;
    fn GetWindowLongPtrW(hwnd: Hwnd, index: i32) -> isize;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(desired_access: u32, inherit_handle: Bool, process_id: u32) -> Handle;
    fn QueryFullProcessImageNameW(process: Handle, flags: u32, executable_name: *mut u16, size: *mut u32) -> Bool;
    fn GetApplicationUserModelId(process: Handle, application_user_model_id_length: *mut u32, application_user_model_id: *mut u16) -> i32;
    fn CloseHandle(handle: Handle) -> Bool;
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
    fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> Bool;
    fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> Bool;
}

#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmGetWindowAttribute(hwnd: Hwnd, attribute: u32, value: *mut c_void, value_size: u32) -> Hresult;
}

#[link(name = "shcore")]
unsafe extern "system" {
    fn GetDpiForMonitor(monitor: Hmonitor, dpi_type: i32, dpi_x: *mut u32, dpi_y: *mut u32) -> Hresult;
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> Hresult;
    fn CoUninitialize();
    fn CoCreateInstance(class_id: *const Guid, outer: *mut c_void, context: u32, interface_id: *const Guid, object: *mut *mut c_void) -> Hresult;
}

#[link(name = "version")]
unsafe extern "system" {
    fn GetFileVersionInfoSizeW(filename: *const u16, handle: *mut u32) -> u32;
    fn GetFileVersionInfoW(filename: *const u16, handle: u32, length: u32, data: *mut c_void) -> Bool;
    fn VerQueryValueW(block: *const c_void, sub_block: *const u16, buffer: *mut *mut c_void, length: *mut u32) -> Bool;
}

#[derive(Debug, Clone)]
struct ProcessEntry { pid: u32, parent_pid: u32, exe_name: String }

#[derive(Debug)]
struct RawWindow {
    hwnd: Hwnd,
    pid: u32,
    title: String,
    z_order: usize,
    taskbar_candidate: bool,
    bounds: Rect,
    restore_bounds: Option<Rect>,
    minimized: bool,
    maximized: bool,
    monitor: usize,
    foreground: bool,
}

struct WindowEnumeration { windows: Vec<RawWindow>, next_z_order: usize, foreground: Hwnd }

#[derive(Debug, Clone)]
struct ProcessMetadata {
    executable_path: Option<String>,
    app_user_model_id: Option<String>,
    file_version: Option<String>,
}

struct ComVirtualDesktopManager { pointer: *mut VirtualDesktopManager, should_uninitialize: bool }

impl ComVirtualDesktopManager {
    fn new() -> Option<Self> {
        let init_result = unsafe { CoInitializeEx(ptr::null_mut(), COINIT_APARTMENTTHREADED) };
        let should_uninitialize = init_result >= 0;
        if init_result < 0 && init_result != RPC_E_CHANGED_MODE { return None; }

        let mut object: *mut c_void = ptr::null_mut();
        let create_result = unsafe {
            CoCreateInstance(
                &CLSID_VIRTUAL_DESKTOP_MANAGER,
                ptr::null_mut(),
                CLSCTX_ALL,
                &IID_VIRTUAL_DESKTOP_MANAGER,
                &mut object,
            )
        };
        if create_result < 0 || object.is_null() {
            if should_uninitialize { unsafe { CoUninitialize() }; }
            return None;
        }

        Some(Self { pointer: object.cast(), should_uninitialize })
    }

    fn describe_window(&self, hwnd: Hwnd) -> (Option<String>, Option<bool>) {
        let vtable = unsafe { &*((*self.pointer).vtable) };
        let mut desktop_id = Guid { data1: 0, data2: 0, data3: 0, data4: [0; 8] };
        let id_result = unsafe { (vtable.get_window_desktop_id)(self.pointer, hwnd, &mut desktop_id) };
        let mut on_current = 0;
        let current_result = unsafe {
            (vtable.is_window_on_current_virtual_desktop)(self.pointer, hwnd, &mut on_current)
        };
        (
            (id_result >= 0).then(|| format_guid(desktop_id)),
            (current_result >= 0).then_some(on_current != 0),
        )
    }
}

impl Drop for ComVirtualDesktopManager {
    fn drop(&mut self) {
        if !self.pointer.is_null() {
            let vtable = unsafe { &*((*self.pointer).vtable) };
            unsafe { (vtable.release)(self.pointer); }
        }
        if self.should_uninitialize { unsafe { CoUninitialize() }; }
    }
}

pub fn discover() -> Result<DesktopSnapshot, String> {
    let mut displays = enumerate_displays()?;
    annotate_display_relations(&mut displays);
    let processes = process_snapshot();
    let raw_windows = enumerate_windows()?;
    let virtual_desktops = ComVirtualDesktopManager::new();

    let mut by_pid: BTreeMap<u32, Vec<RawWindow>> = BTreeMap::new();
    for window in raw_windows { by_pid.entry(window.pid).or_default().push(window); }
    for process in processes.values() {
        if is_known_background_app(&process.exe_name) { by_pid.entry(process.pid).or_default(); }
    }

    let mut applications = Vec::new();
    let mut ignored = Vec::new();

    for (pid, mut windows) in by_pid {
        windows.sort_by_key(|window| window.z_order);
        let process = processes.get(&pid);
        let metadata = query_process_metadata(pid);
        let executable_path = metadata.executable_path.clone();
        let executable = process
            .map(|entry| entry.exe_name.clone())
            .filter(|name| !name.is_empty())
            .or_else(|| executable_path.as_deref().and_then(file_name))
            .unwrap_or_else(|| "unknown.exe".to_owned());

        let known_background = is_known_background_app(&executable);
        let titles: Vec<String> = windows.iter().map(|window| window.title.clone()).collect();
        let has_taskbar_candidate = windows.iter().any(|window| window.taskbar_candidate);
        let decision = classify_candidate(&executable, &titles, has_taskbar_candidate, known_background);

        if decision.classification != ApplicationClassification::UserApplication {
            ignored.push(IgnoredCandidate {
                pid,
                parent_pid: process.map(|entry| entry.parent_pid).filter(|parent| *parent != 0),
                executable,
                executable_path,
                window_title: windows.first().map(|window| window.title.clone()),
                classification: decision.classification,
                confidence: decision.confidence,
                reason: decision.reason,
            });
            continue;
        }

        if executable.eq_ignore_ascii_case("explorer.exe") && windows.len() > 1 {
            let mut kept = Vec::new();
            for window in windows {
                if window.title.eq_ignore_ascii_case("Program Manager") {
                    ignored.push(IgnoredCandidate {
                        pid,
                        parent_pid: process.map(|entry| entry.parent_pid).filter(|parent| *parent != 0),
                        executable: executable.clone(),
                        executable_path: executable_path.clone(),
                        window_title: Some(window.title),
                        classification: ApplicationClassification::ShellComponent,
                        confidence: 100,
                        reason: "Windows desktop shell window (Program Manager)".to_owned(),
                    });
                } else { kept.push(window); }
            }
            windows = kept;
        }

        let window_infos = windows
            .into_iter()
            .filter_map(|window| build_window_info(window, &displays, virtual_desktops.as_ref()))
            .collect::<Vec<_>>();
        let launch = build_launch_spec(metadata.app_user_model_id.as_deref(), executable_path.as_deref());
        let name = executable_path
            .as_deref()
            .and_then(application_name)
            .unwrap_or_else(|| executable.strip_suffix(".exe").unwrap_or(&executable).to_owned());

        applications.push(ApplicationInfo {
            primary_pid: pid,
            pids: vec![pid],
            parent_pid: process.map(|entry| entry.parent_pid).filter(|parent| *parent != 0),
            name,
            executable_path,
            app_user_model_id: metadata.app_user_model_id,
            file_version: metadata.file_version,
            classification: decision.classification,
            confidence: decision.confidence,
            classification_reason: decision.reason,
            launch,
            discovered_as_background: window_infos.is_empty(),
            windows: window_infos,
        });
    }

    applications = merge_applications(applications);
    applications.sort_by(|left, right| {
        application_z_order(left)
            .cmp(&application_z_order(right))
            .then_with(|| left.name.to_ascii_lowercase().cmp(&right.name.to_ascii_lowercase()))
    });
    ignored.sort_by(|left, right| {
        left.executable.to_ascii_lowercase().cmp(&right.executable.to_ascii_lowercase()).then_with(|| left.pid.cmp(&right.pid))
    });

    Ok(DesktopSnapshot { displays, applications, ignored })
}

fn enumerate_displays() -> Result<Vec<DisplayInfo>, String> {
    let mut displays: Vec<DisplayInfo> = Vec::new();
    let data = (&mut displays as *mut Vec<DisplayInfo>) as isize;
    let result = unsafe { EnumDisplayMonitors(ptr::null_mut(), ptr::null(), Some(enum_monitor), data) };
    if result == 0 { return Err("failed to enumerate Windows displays".to_owned()); }
    displays.sort_by(|left, right| {
        right.is_primary.cmp(&left.is_primary).then_with(|| left.bounds.left.cmp(&right.bounds.left)).then_with(|| left.bounds.top.cmp(&right.bounds.top))
    });
    Ok(displays)
}

unsafe extern "system" fn enum_monitor(monitor: Hmonitor, _hdc: Hdc, _rect: *mut NativeRect, data: isize) -> Bool {
    let Some(info) = monitor_info(monitor) else { return 1; };
    let displays = unsafe { &mut *(data as *mut Vec<DisplayInfo>) };
    displays.push(info);
    1
}

fn monitor_info(monitor: Hmonitor) -> Option<DisplayInfo> {
    if monitor.is_null() { return None; }
    let mut info = MonitorInfoExW {
        size: size_of::<MonitorInfoExW>() as u32,
        monitor: NativeRect::default(),
        work: NativeRect::default(),
        flags: 0,
        device: [0; 32],
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 { return None; }
    let bounds = Rect::from(info.monitor);
    let orientation = if bounds.width() >= bounds.height() { "landscape" } else { "portrait" };
    Some(DisplayInfo {
        device_name: wide_buffer_to_string(&info.device),
        bounds,
        work_area: Rect::from(info.work),
        is_primary: info.flags & MONITORINFOF_PRIMARY != 0,
        scale_percent: monitor_scale_percent(monitor),
        orientation,
        relation_to_primary: String::new(),
    })
}

fn annotate_display_relations(displays: &mut [DisplayInfo]) {
    let Some(primary_bounds) = displays.iter().find(|display| display.is_primary).map(|display| display.bounds).or_else(|| displays.first().map(|display| display.bounds)) else { return; };
    for display in displays {
        display.relation_to_primary = display_relation(display.bounds, primary_bounds, display.is_primary);
    }
}

fn monitor_scale_percent(monitor: Hmonitor) -> u32 {
    let mut dpi_x = 96_u32;
    let mut dpi_y = 96_u32;
    let result = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    if result < 0 || dpi_x == 0 { 100 } else { ((dpi_x as f64 / 96.0) * 100.0).round() as u32 }
}

fn enumerate_windows() -> Result<Vec<RawWindow>, String> {
    let foreground = unsafe { GetForegroundWindow() };
    let mut context = WindowEnumeration { windows: Vec::new(), next_z_order: 0, foreground };
    let data = (&mut context as *mut WindowEnumeration) as isize;
    let result = unsafe { EnumWindows(Some(enum_window), data) };
    if result == 0 { return Err("failed to enumerate Windows top-level windows".to_owned()); }
    Ok(context.windows)
}

unsafe extern "system" fn enum_window(hwnd: Hwnd, data: isize) -> Bool {
    if unsafe { IsWindowVisible(hwnd) } == 0 || is_cloaked(hwnd) { return 1; }
    let title = window_title(hwnd);
    if title.is_empty() { return 1; }
    let mut pid = 0_u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid); }
    if pid == 0 { return 1; }
    let Some(bounds) = window_bounds(hwnd) else { return 1; };
    let restore_bounds = window_restore_bounds(hwnd);
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let taskbar_candidate = is_taskbar_candidate(hwnd);
    let minimized = unsafe { IsIconic(hwnd) != 0 };
    let maximized = unsafe { IsZoomed(hwnd) != 0 };
    let context = unsafe { &mut *(data as *mut WindowEnumeration) };
    let z_order = context.next_z_order;
    context.next_z_order += 1;
    context.windows.push(RawWindow {
        hwnd, pid, title, z_order, taskbar_candidate, bounds, restore_bounds, minimized, maximized,
        monitor: monitor as usize, foreground: hwnd == context.foreground,
    });
    1
}

fn window_title(hwnd: Hwnd) -> String {
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 { return String::new(); }
    let mut buffer = vec![0_u16; length as usize + 1];
    let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied <= 0 { return String::new(); }
    String::from_utf16_lossy(&buffer[..copied as usize]).trim().to_owned()
}

fn is_cloaked(hwnd: Hwnd) -> bool {
    let mut cloaked = 0_u32;
    let result = unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), size_of::<u32>() as u32)
    };
    result >= 0 && cloaked != 0
}

fn window_bounds(hwnd: Hwnd) -> Option<Rect> {
    let mut rect = NativeRect::default();
    let dwm_result = unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, (&mut rect as *mut NativeRect).cast(), size_of::<NativeRect>() as u32)
    };
    if dwm_result >= 0 { return Some(rect.into()); }
    if unsafe { GetWindowRect(hwnd, &mut rect) } != 0 { Some(rect.into()) } else { None }
}

fn window_restore_bounds(hwnd: Hwnd) -> Option<Rect> {
    let mut placement: WindowPlacement = unsafe { zeroed() };
    placement.length = size_of::<WindowPlacement>() as u32;
    if unsafe { GetWindowPlacement(hwnd, &mut placement) } == 0 { return None; }
    let rect = Rect::from(placement.normal_position);
    (rect.width() > 0 && rect.height() > 0).then_some(rect)
}

fn is_taskbar_candidate(hwnd: Hwnd) -> bool {
    let owner = unsafe { GetWindow(hwnd, GW_OWNER) };
    let extended_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    let forced_app = extended_style & WS_EX_APPWINDOW != 0;
    let tool_window = extended_style & WS_EX_TOOLWINDOW != 0;
    forced_app || (owner.is_null() && !tool_window)
}

fn build_window_info(window: RawWindow, displays: &[DisplayInfo], virtual_desktops: Option<&ComVirtualDesktopManager>) -> Option<WindowInfo> {
    let monitor = window.monitor as Hmonitor;
    let device = monitor_info(monitor).map(|info| info.device_name);
    let display = device.as_deref().and_then(|device| displays.iter().find(|display| display.device_name == device)).or_else(|| displays.first())?;
    let geometry_bounds = if window.minimized { window.restore_bounds.unwrap_or(window.bounds) } else { window.bounds };
    let normalized = NormalizedRect::from_rect(geometry_bounds, display.work_area);
    let state = if window.minimized {
        WindowState::Minimized
    } else if rect_matches(window.bounds, display.bounds, 4) {
        WindowState::Fullscreen
    } else if window.maximized {
        WindowState::Maximized
    } else if let Some(snap) = normalized.and_then(detect_snap) {
        WindowState::Snapped(snap)
    } else {
        WindowState::Normal
    };
    let (virtual_desktop_id, on_current_desktop) = virtual_desktops.map(|manager| manager.describe_window(window.hwnd)).unwrap_or((None, None));
    Some(WindowInfo {
        title: window.title,
        bounds: window.bounds,
        restore_bounds: window.restore_bounds.filter(|rect| *rect != window.bounds),
        normalized_bounds: normalized,
        state,
        display_device: display.device_name.clone(),
        display_relation: display.relation_to_primary.clone(),
        display_scale_percent: display.scale_percent,
        is_foreground: window.foreground,
        z_order: window.z_order,
        virtual_desktop_id,
        is_on_current_virtual_desktop: on_current_desktop,
        taskbar_candidate: window.taskbar_candidate,
    })
}

fn rect_matches(left: Rect, right: Rect, tolerance: i32) -> bool {
    (left.left - right.left).abs() <= tolerance
        && (left.top - right.top).abs() <= tolerance
        && (left.right - right.right).abs() <= tolerance
        && (left.bottom - right.bottom).abs() <= tolerance
}

fn process_snapshot() -> HashMap<u32, ProcessEntry> {
    let mut result = HashMap::new();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot as isize == -1 { return result; }
    let mut entry: ProcessEntry32W = unsafe { zeroed() };
    entry.size = size_of::<ProcessEntry32W>() as u32;
    let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while has_entry {
        let process = ProcessEntry { pid: entry.process_id, parent_pid: entry.parent_process_id, exe_name: wide_buffer_to_string(&entry.exe_file) };
        result.insert(process.pid, process);
        entry = unsafe { zeroed() };
        entry.size = size_of::<ProcessEntry32W>() as u32;
        has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot); }
    result
}

fn query_process_metadata(pid: u32) -> ProcessMetadata {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return ProcessMetadata { executable_path: None, app_user_model_id: None, file_version: None };
    }
    let executable_path = query_process_path(process);
    let app_user_model_id = query_app_user_model_id(process);
    unsafe { CloseHandle(process); }
    let file_version = executable_path.as_deref().and_then(query_file_version);
    ProcessMetadata { executable_path, app_user_model_id, file_version }
}

fn query_process_path(process: Handle) -> Option<String> {
    let mut buffer = vec![0_u16; 32_768];
    let mut size = buffer.len() as u32;
    let result = unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut size) };
    if result == 0 || size == 0 { None } else { Some(String::from_utf16_lossy(&buffer[..size as usize])) }
}

fn query_app_user_model_id(process: Handle) -> Option<String> {
    let mut length = 0_u32;
    let first_result = unsafe { GetApplicationUserModelId(process, &mut length, ptr::null_mut()) };
    if first_result != ERROR_INSUFFICIENT_BUFFER || length == 0 { return None; }
    let mut buffer = vec![0_u16; length as usize];
    let second_result = unsafe { GetApplicationUserModelId(process, &mut length, buffer.as_mut_ptr()) };
    if second_result != 0 || length == 0 { return None; }
    let value = wide_buffer_to_string(&buffer);
    (!value.is_empty()).then_some(value)
}

fn query_file_version(path: &str) -> Option<String> {
    let wide_path = to_wide(path);
    let mut ignored_handle = 0_u32;
    let size = unsafe { GetFileVersionInfoSizeW(wide_path.as_ptr(), &mut ignored_handle) };
    if size == 0 { return None; }
    let mut data = vec![0_u8; size as usize];
    if unsafe { GetFileVersionInfoW(wide_path.as_ptr(), 0, size, data.as_mut_ptr().cast()) } == 0 { return None; }
    let sub_block = [b'\\' as u16, 0];
    let mut buffer: *mut c_void = ptr::null_mut();
    let mut length = 0_u32;
    if unsafe { VerQueryValueW(data.as_ptr().cast(), sub_block.as_ptr(), &mut buffer, &mut length) } == 0
        || buffer.is_null() || length < size_of::<VsFixedFileInfo>() as u32 { return None; }
    let info = unsafe { &*(buffer as *const VsFixedFileInfo) };
    let major = info.file_version_ms >> 16;
    let minor = info.file_version_ms & 0xffff;
    let build = info.file_version_ls >> 16;
    let revision = info.file_version_ls & 0xffff;
    Some(format!("{major}.{minor}.{build}.{revision}"))
}

fn build_launch_spec(app_user_model_id: Option<&str>, executable_path: Option<&str>) -> Option<LaunchSpec> {
    if let Some(application_id) = app_user_model_id {
        return Some(LaunchSpec { strategy: LaunchStrategy::AppUserModelId, target: application_id.to_owned() });
    }
    executable_path.map(|path| LaunchSpec { strategy: LaunchStrategy::Executable, target: path.to_owned() })
}

fn application_name(path: &str) -> Option<String> {
    Path::new(path).file_stem().map(|name| name.to_string_lossy().into_owned())
}

fn file_name(path: &str) -> Option<String> {
    Path::new(path).file_name().map(|name| name.to_string_lossy().into_owned())
}

fn merge_applications(applications: Vec<ApplicationInfo>) -> Vec<ApplicationInfo> {
    let mut merged: BTreeMap<String, ApplicationInfo> = BTreeMap::new();
    for mut application in applications {
        let key = if let Some(application_id) = application.app_user_model_id.as_ref() {
            format!("aumid:{}", application_id.to_ascii_lowercase())
        } else if let Some(path) = application.executable_path.as_ref() {
            format!("exe:{}", path.to_ascii_lowercase())
        } else {
            format!("pid:{}:{}", application.primary_pid, application.name.to_ascii_lowercase())
        };
        if let Some(existing) = merged.get_mut(&key) {
            existing.pids.append(&mut application.pids);
            existing.windows.append(&mut application.windows);
            existing.pids.sort_unstable();
            existing.pids.dedup();
            existing.windows.sort_by_key(|window| window.z_order);
            existing.primary_pid = existing.pids[0];
            existing.discovered_as_background = existing.windows.is_empty();
            existing.confidence = existing.confidence.max(application.confidence);
        } else { merged.insert(key, application); }
    }
    merged.into_values().collect()
}

fn application_z_order(application: &ApplicationInfo) -> usize {
    application.windows.iter().map(|window| window.z_order).min().unwrap_or(usize::MAX)
}

fn wide_buffer_to_string(buffer: &[u16]) -> String {
    let length = buffer.iter().position(|value| *value == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

fn to_wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(std::iter::once(0)).collect()
}

fn format_guid(guid: Guid) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        guid.data1, guid.data2, guid.data3, guid.data4[0], guid.data4[1], guid.data4[2], guid.data4[3],
        guid.data4[4], guid.data4[5], guid.data4[6], guid.data4[7],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_prefers_app_user_model_id() {
        let launch = build_launch_spec(Some("Contoso.App_123!App"), Some(r"C:\\Program Files\\Contoso\\app.exe")).expect("launch spec");
        assert_eq!(launch.strategy, LaunchStrategy::AppUserModelId);
        assert_eq!(launch.target, "Contoso.App_123!App");
    }

    #[test]
    fn launcher_falls_back_to_executable_path() {
        let launch = build_launch_spec(None, Some(r"C:\\Program Files\\Contoso\\app.exe")).expect("launch spec");
        assert_eq!(launch.strategy, LaunchStrategy::Executable);
    }

    #[test]
    fn fullscreen_bounds_allow_small_border_variance() {
        let display = Rect { left: 0, top: 0, right: 1920, bottom: 1080 };
        let window = Rect { left: -2, top: 0, right: 1922, bottom: 1081 };
        assert!(rect_matches(window, display, 4));
    }
}
