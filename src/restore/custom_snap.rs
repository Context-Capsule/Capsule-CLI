use super::{
    SavedApplication, SavedDesktop, SavedDisplay, SavedNormalizedRect, SavedRect, SavedWindow,
    SnapSlot, WindowStateSpec, title_match_score,
};
use crate::windows_snap::{self, SplitOrientation};
use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    mem::size_of,
    path::Path,
};

type Hwnd = *mut c_void;
type Handle = *mut c_void;
type Hmonitor = *mut c_void;
type Bool = i32;
type Hresult = i32;

type EnumWindowsProc = Option<unsafe extern "system" fn(Hwnd, isize) -> Bool>;

const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const MONITOR_DEFAULTTONEAREST: u32 = 2;
const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;
const LAYOUT_TOLERANCE: f64 = 0.035;
const PORTRAIT_PAIR_TOLERANCE: i64 = 3;

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

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(callback: EnumWindowsProc, lparam: isize) -> Bool;
    fn IsWindowVisible(hwnd: Hwnd) -> Bool;
    fn GetWindowTextLengthW(hwnd: Hwnd) -> i32;
    fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, max_count: i32) -> i32;
    fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
    fn GetWindowRect(hwnd: Hwnd, rect: *mut NativeRect) -> Bool;
    fn MonitorFromWindow(hwnd: Hwnd, flags: u32) -> Hmonitor;
    fn GetMonitorInfoW(monitor: Hmonitor, info: *mut MonitorInfoExW) -> Bool;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(desired_access: u32, inherit_handle: Bool, process_id: u32) -> Handle;
    fn QueryFullProcessImageNameW(
        process: Handle,
        flags: u32,
        executable_name: *mut u16,
        size: *mut u32,
    ) -> Bool;
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

#[derive(Debug, Default)]
pub(super) struct CustomSnapRestoreReport {
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

#[derive(Clone, Copy)]
struct SavedCustom<'a> {
    app: &'a SavedApplication,
    window: &'a SavedWindow,
    normalized: SavedNormalizedRect,
}

#[derive(Clone, Copy)]
struct SavedPair<'a> {
    first: SavedCustom<'a>,
    second: SavedCustom<'a>,
    orientation: SplitOrientation,
    divider_fraction: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SavedStockPairIndex {
    top_app: usize,
    top_window: usize,
    bottom_app: usize,
    bottom_window: usize,
}

#[derive(Debug, Clone)]
struct CurrentWindow {
    hwnd: usize,
    title: String,
    bounds: SavedRect,
    executable_path: Option<String>,
    monitor: MonitorInfo,
}

#[derive(Debug, Clone)]
struct MonitorInfo {
    device_name: String,
    work_area: SavedRect,
}

struct WindowEnumeration {
    windows: Vec<CurrentWindowRaw>,
}

struct CurrentWindowRaw {
    hwnd: usize,
    pid: u32,
    title: String,
    bounds: SavedRect,
    monitor: MonitorInfo,
}

/// Rebuilds custom two-window Snap layouts after the generic placement pass.
///
/// A custom split means Windows reported both windows as arranged at capture,
/// but their divider no longer matched a stock fraction. The generic pass first
/// puts those windows on the correct monitor/geometry. This stage then matches
/// the live HWNDs and asks `windows_snap` to create a real native pair and replay
/// the divider drag. Unsupported multi-window custom grids remain exact floating
/// geometry for now and are reported explicitly rather than silently claimed as
/// native Snap restoration.
pub(super) fn restore(desktop: &SavedDesktop) -> CustomSnapRestoreReport {
    let mut report = CustomSnapRestoreReport::default();
    let customs = saved_custom_windows(desktop);
    if customs.is_empty() {
        return report;
    }

    let pairs = build_pairs(&customs, &mut report.warnings);
    if pairs.is_empty() {
        return report;
    }

    let current = match enumerate_windows() {
        Ok(windows) => windows,
        Err(error) => {
            report
                .failures
                .push(format!("custom native snap inventory failed: {error}"));
            return report;
        }
    };
    let mut used = HashSet::new();

    for pair in pairs {
        let Some(first) = match_saved_window(pair.first, &current, &used) else {
            report.failures.push(format!(
                "custom native snap restore could not find the current window for '{}'",
                pair.first.window.title
            ));
            continue;
        };
        used.insert(first.hwnd);

        let Some(second) = match_saved_window(pair.second, &current, &used) else {
            used.remove(&first.hwnd);
            report.failures.push(format!(
                "custom native snap restore could not find the current window for '{}'",
                pair.second.window.title
            ));
            continue;
        };
        used.insert(second.hwnd);

        if !first
            .monitor
            .device_name
            .eq_ignore_ascii_case(&second.monitor.device_name)
        {
            report.failures.push(format!(
                "custom native snap pair '{}' / '{}' landed on different monitors before native grouping",
                pair.first.window.title, pair.second.window.title
            ));
            continue;
        }

        let work = first.monitor.work_area;
        match windows_snap::restore_resized_pair(
            first.hwnd,
            second.hwnd,
            pair.orientation,
            [work.left, work.top, work.right, work.bottom],
            pair.divider_fraction,
        ) {
            Ok(()) => {}
            Err(error) => report.failures.push(format!(
                "custom native snap restore failed for '{}' + '{}': {error}",
                pair.first.window.title, pair.second.window.title
            )),
        }
    }

    report
}

/// Returns a placement-only clone for a complete, unambiguous top/bottom pair
/// captured on a portrait monitor. Generic placement restores the exact two
/// non-overlapping rectangles without independently injecting TopHalf/BottomHalf
/// shortcuts. The original desktop remains authoritative for native pair replay.
pub(super) fn geometry_only_portrait_stacked_pairs(desktop: &SavedDesktop) -> SavedDesktop {
    let pairs = portrait_stacked_pair_indices(desktop);
    if pairs.is_empty() {
        return desktop.clone();
    }

    let mut placement = desktop.clone();
    for pair in pairs {
        placement.applications[pair.top_app].windows[pair.top_window].state = "normal".to_owned();
        placement.applications[pair.bottom_app].windows[pair.bottom_window].state =
            "normal".to_owned();
    }
    placement
}

/// Rebuilds a canonical portrait 50/50 top/bottom layout as one native pair.
/// Each shortcut is attempted once; an unchanged failure is reported immediately.
pub(super) fn restore_portrait_stacked_pairs(desktop: &SavedDesktop) -> CustomSnapRestoreReport {
    let mut report = CustomSnapRestoreReport::default();
    let pairs = portrait_stacked_pair_indices(desktop);
    if pairs.is_empty() {
        return report;
    }

    let current = match enumerate_windows() {
        Ok(windows) => windows,
        Err(error) => {
            report
                .failures
                .push(format!("portrait stock snap inventory failed: {error}"));
            return report;
        }
    };
    let mut used = HashSet::new();

    for pair in pairs {
        let top_app = &desktop.applications[pair.top_app];
        let top_window = &top_app.windows[pair.top_window];
        let bottom_app = &desktop.applications[pair.bottom_app];
        let bottom_window = &bottom_app.windows[pair.bottom_window];

        let top_saved = SavedCustom {
            app: top_app,
            window: top_window,
            normalized: stock_normalized(SnapSlot::TopHalf),
        };
        let bottom_saved = SavedCustom {
            app: bottom_app,
            window: bottom_window,
            normalized: stock_normalized(SnapSlot::BottomHalf),
        };

        let Some(top) = match_saved_window(top_saved, &current, &used) else {
            report.failures.push(format!(
                "portrait stock snap restore could not find the current top-half window '{}'",
                top_window.title
            ));
            continue;
        };
        used.insert(top.hwnd);

        let Some(bottom) = match_saved_window(bottom_saved, &current, &used) else {
            used.remove(&top.hwnd);
            report.failures.push(format!(
                "portrait stock snap restore could not find the current bottom-half window '{}'",
                bottom_window.title
            ));
            continue;
        };
        used.insert(bottom.hwnd);

        if !top
            .monitor
            .device_name
            .eq_ignore_ascii_case(&bottom.monitor.device_name)
        {
            report.failures.push(format!(
                "portrait stock snap pair '{}' / '{}' landed on different monitors before native grouping",
                top_window.title, bottom_window.title
            ));
            continue;
        }

        let work = top.monitor.work_area;
        if work.width() >= work.height() {
            report.failures.push(format!(
                "saved portrait stock snap pair '{}' / '{}' resolved to a non-portrait monitor at restore time",
                top_window.title, bottom_window.title
            ));
            continue;
        }

        let Some(top_target) = normalized_target(work, top_saved.normalized) else {
            report.failures.push(format!(
                "portrait stock snap restore could not calculate the saved top-half target for '{}'",
                top_window.title
            ));
            continue;
        };
        let Some(bottom_target) = normalized_target(work, bottom_saved.normalized) else {
            report.failures.push(format!(
                "portrait stock snap restore could not calculate the saved bottom-half target for '{}'",
                bottom_window.title
            ));
            continue;
        };

        if portrait_pair_is_exact_native(top.hwnd, bottom.hwnd, top_target, bottom_target) {
            continue;
        }

        let native_result = windows_snap::restore_equal_pair(
            top.hwnd,
            bottom.hwnd,
            SplitOrientation::Stacked,
            [work.left, work.top, work.right, work.bottom],
        );
        if portrait_pair_is_exact_native(top.hwnd, bottom.hwnd, top_target, bottom_target) {
            continue;
        }

        let native_detail = native_result
            .err()
            .unwrap_or_else(|| "Windows reported native Snap success, but the settled pair did not match the saved 50/50 halves".to_owned());

        match fallback_portrait_pair_to_geometry(
            top.hwnd,
            bottom.hwnd,
            top_target,
            bottom_target,
        ) {
            Ok(()) => report.warnings.push(format!(
                "portrait native Snap was not exact for '{}' + '{}' ({native_detail}); retained the saved top/bottom halves as exact floating geometry instead",
                top_window.title, bottom_window.title
            )),
            Err(fallback_error) => report.failures.push(format!(
                "portrait restore failed for '{}' + '{}': native attempt: {native_detail}; floating fallback: {fallback_error}",
                top_window.title, bottom_window.title
            )),
        }
    }

    report
}

fn portrait_pair_is_exact_native(
    top_hwnd: usize,
    bottom_hwnd: usize,
    top_target: SavedRect,
    bottom_target: SavedRect,
) -> bool {
    windows_snap::is_arranged(top_hwnd as Hwnd) == Some(true)
        && windows_snap::is_arranged(bottom_hwnd as Hwnd) == Some(true)
        && window_bounds(top_hwnd as Hwnd)
            .is_some_and(|bounds| rect_distance(bounds, top_target) <= PORTRAIT_PAIR_TOLERANCE)
        && window_bounds(bottom_hwnd as Hwnd)
            .is_some_and(|bounds| rect_distance(bounds, bottom_target) <= PORTRAIT_PAIR_TOLERANCE)
}

fn fallback_portrait_pair_to_geometry(
    top_hwnd: usize,
    bottom_hwnd: usize,
    top_target: SavedRect,
    bottom_target: SavedRect,
) -> Result<(), String> {
    let top_observed = super::windows::force_floating_geometry(top_hwnd, top_target)?;
    let bottom_observed = super::windows::force_floating_geometry(bottom_hwnd, bottom_target)?;

    if rect_distance(top_observed, top_target) > PORTRAIT_PAIR_TOLERANCE {
        return Err(format!(
            "top floating fallback missed the saved half: observed left={} top={} right={} bottom={}, target left={} top={} right={} bottom={}",
            top_observed.left,
            top_observed.top,
            top_observed.right,
            top_observed.bottom,
            top_target.left,
            top_target.top,
            top_target.right,
            top_target.bottom,
        ));
    }
    if rect_distance(bottom_observed, bottom_target) > PORTRAIT_PAIR_TOLERANCE {
        return Err(format!(
            "bottom floating fallback missed the saved half: observed left={} top={} right={} bottom={}, target left={} top={} right={} bottom={}",
            bottom_observed.left,
            bottom_observed.top,
            bottom_observed.right,
            bottom_observed.bottom,
            bottom_target.left,
            bottom_target.top,
            bottom_target.right,
            bottom_target.bottom,
        ));
    }
    Ok(())
}

fn portrait_stacked_pair_indices(desktop: &SavedDesktop) -> Vec<SavedStockPairIndex> {
    let mut tops = Vec::new();
    let mut bottoms = Vec::new();

    for (app_index, app) in desktop.applications.iter().enumerate() {
        for (window_index, window) in app.windows.iter().enumerate() {
            if !saved_window_is_on_portrait_display(desktop, window) {
                continue;
            }
            match window.state_spec() {
                WindowStateSpec::Snapped(SnapSlot::TopHalf) => {
                    tops.push((app_index, window_index, window));
                }
                WindowStateSpec::Snapped(SnapSlot::BottomHalf) => {
                    bottoms.push((app_index, window_index, window));
                }
                _ => {}
            }
        }
    }

    let mut result = Vec::new();
    for (top_app, top_window, top) in &tops {
        let compatible_bottoms = bottoms
            .iter()
            .filter(|(_, _, bottom)| same_saved_display(top, bottom))
            .collect::<Vec<_>>();
        if compatible_bottoms.len() != 1 {
            continue;
        }
        let (bottom_app, bottom_window, bottom) = *compatible_bottoms[0];
        let compatible_tops = tops
            .iter()
            .filter(|(_, _, candidate)| same_saved_display(candidate, bottom))
            .count();
        if compatible_tops != 1 {
            continue;
        }
        result.push(SavedStockPairIndex {
            top_app: *top_app,
            top_window: *top_window,
            bottom_app,
            bottom_window,
        });
    }
    result
}

fn saved_window_is_on_portrait_display(desktop: &SavedDesktop, window: &SavedWindow) -> bool {
    desktop
        .displays
        .iter()
        .find(|display| {
            display
                .device_name
                .eq_ignore_ascii_case(&window.display_device)
                || (!window.display_relation.is_empty()
                    && display
                        .relation_to_primary
                        .eq_ignore_ascii_case(&window.display_relation))
        })
        .is_some_and(|display| display.work_area.height() > display.work_area.width())
}

fn stock_normalized(slot: SnapSlot) -> SavedNormalizedRect {
    match slot {
        SnapSlot::TopHalf => SavedNormalizedRect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 0.5,
        },
        SnapSlot::BottomHalf => SavedNormalizedRect {
            x: 0.0,
            y: 0.5,
            width: 1.0,
            height: 0.5,
        },
        _ => unreachable!("stock portrait pair only supports top/bottom halves"),
    }
}

fn saved_custom_windows(desktop: &SavedDesktop) -> Vec<SavedCustom<'_>> {
    desktop
        .applications
        .iter()
        .flat_map(|app| {
            app.windows.iter().filter_map(move |window| {
                if window.state != "snapped:custom" {
                    return None;
                }
                window.normalized_bounds.map(|normalized| SavedCustom {
                    app,
                    window,
                    normalized,
                })
            })
        })
        .collect()
}

fn build_pairs<'a>(customs: &[SavedCustom<'a>], warnings: &mut Vec<String>) -> Vec<SavedPair<'a>> {
    let mut result = Vec::new();
    let mut used = vec![false; customs.len()];

    for i in 0..customs.len() {
        if used[i] {
            continue;
        }
        let mut found = None;
        for j in (i + 1)..customs.len() {
            if used[j] {
                continue;
            }
            if let Some(pair) = pair_shape(customs[i], customs[j]) {
                found = Some((j, pair));
                break;
            }
        }

        if let Some((j, pair)) = found {
            used[i] = true;
            used[j] = true;
            result.push(pair);
        } else {
            warnings.push(format!(
                "Custom snapped window '{}' is not part of a supported two-window full-width/full-height split; its exact rectangle was restored, but native Snap grouping was not attempted",
                customs[i].window.title
            ));
        }
    }

    result
}

fn pair_shape<'a>(a: SavedCustom<'a>, b: SavedCustom<'a>) -> Option<SavedPair<'a>> {
    if !same_saved_display(a.window, b.window) {
        return None;
    }

    let (left, right) = if a.normalized.x <= b.normalized.x {
        (a, b)
    } else {
        (b, a)
    };
    if approx(left.normalized.y, 0.0)
        && approx(right.normalized.y, 0.0)
        && approx(left.normalized.height, 1.0)
        && approx(right.normalized.height, 1.0)
        && approx(left.normalized.x, 0.0)
        && approx(right.normalized.x + right.normalized.width, 1.0)
        && approx(
            left.normalized.x + left.normalized.width,
            right.normalized.x,
        )
    {
        let divider = ((left.normalized.x + left.normalized.width) + right.normalized.x) / 2.0;
        if (0.05..=0.95).contains(&divider) {
            return Some(SavedPair {
                first: left,
                second: right,
                orientation: SplitOrientation::SideBySide,
                divider_fraction: divider,
            });
        }
    }

    let (top, bottom) = if a.normalized.y <= b.normalized.y {
        (a, b)
    } else {
        (b, a)
    };
    if approx(top.normalized.x, 0.0)
        && approx(bottom.normalized.x, 0.0)
        && approx(top.normalized.width, 1.0)
        && approx(bottom.normalized.width, 1.0)
        && approx(top.normalized.y, 0.0)
        && approx(bottom.normalized.y + bottom.normalized.height, 1.0)
        && approx(
            top.normalized.y + top.normalized.height,
            bottom.normalized.y,
        )
    {
        let divider = ((top.normalized.y + top.normalized.height) + bottom.normalized.y) / 2.0;
        if (0.05..=0.95).contains(&divider) {
            return Some(SavedPair {
                first: top,
                second: bottom,
                orientation: SplitOrientation::Stacked,
                divider_fraction: divider,
            });
        }
    }

    None
}

fn same_saved_display(left: &SavedWindow, right: &SavedWindow) -> bool {
    left.display_device
        .eq_ignore_ascii_case(&right.display_device)
        || (!left.display_relation.is_empty()
            && left
                .display_relation
                .eq_ignore_ascii_case(&right.display_relation))
}

fn approx(actual: f64, expected: f64) -> bool {
    actual.is_finite() && (actual - expected).abs() <= LAYOUT_TOLERANCE
}

fn match_saved_window<'a>(
    saved: SavedCustom<'_>,
    current: &'a [CurrentWindow],
    used: &HashSet<usize>,
) -> Option<&'a CurrentWindow> {
    current
        .iter()
        .filter(|candidate| !used.contains(&candidate.hwnd))
        .filter_map(|candidate| {
            let identity = application_match_score(saved.app, candidate)?;
            let title = title_match_score(&saved.window.title, &candidate.title) as i64;
            let target = normalized_target(candidate.monitor.work_area, saved.normalized)?;
            let distance = rect_distance(target, candidate.bounds).min(20_000);
            let score = identity * 100_000 + title * 1_000 - distance;
            Some((score, candidate))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, candidate)| candidate)
}

fn application_match_score(app: &SavedApplication, current: &CurrentWindow) -> Option<i64> {
    let current_path = current.executable_path.as_deref();

    if let (Some(saved), Some(current)) = (app.executable_path.as_deref(), current_path) {
        if normalize_path(saved) == normalize_path(current) {
            return Some(100);
        }
    }

    if let Some(launch) = app
        .launch
        .as_ref()
        .filter(|launch| launch.strategy == "executable")
    {
        if let Some(current) = current_path {
            if normalize_path(&launch.target) == normalize_path(current) {
                return Some(95);
            }
        }
    }

    let current_name = current_path.and_then(file_name).unwrap_or("");
    if let Some(saved_name) = app.executable_path.as_deref().and_then(file_name) {
        if !current_name.is_empty() && current_name.eq_ignore_ascii_case(saved_name) {
            return Some(80);
        }
    }

    let current_stem = current_name.strip_suffix(".exe").unwrap_or(current_name);
    if !current_stem.is_empty() && current_stem.eq_ignore_ascii_case(&app.name) {
        return Some(70);
    }

    (title_match_score(&app.name, &current.title) >= 60).then_some(40)
}

fn normalized_target(area: SavedRect, normalized: SavedNormalizedRect) -> Option<SavedRect> {
    if !normalized.x.is_finite()
        || !normalized.y.is_finite()
        || !normalized.width.is_finite()
        || !normalized.height.is_finite()
        || normalized.width <= 0.0
        || normalized.height <= 0.0
        || area.right <= area.left
        || area.bottom <= area.top
    {
        return None;
    }
    let width = area.right - area.left;
    let height = area.bottom - area.top;
    let left = area.left + (normalized.x * width as f64).round() as i32;
    let top = area.top + (normalized.y * height as f64).round() as i32;
    let right = left + (normalized.width * width as f64).round() as i32;
    let bottom = top + (normalized.height * height as f64).round() as i32;
    Some(SavedRect {
        left,
        top,
        right,
        bottom,
    })
}

fn rect_distance(left: SavedRect, right: SavedRect) -> i64 {
    (left.left - right.left).abs() as i64
        + (left.top - right.top).abs() as i64
        + (left.right - right.right).abs() as i64
        + (left.bottom - right.bottom).abs() as i64
}

fn enumerate_windows() -> Result<Vec<CurrentWindow>, String> {
    let mut context = WindowEnumeration {
        windows: Vec::new(),
    };
    if unsafe {
        EnumWindows(
            Some(enum_window),
            (&mut context as *mut WindowEnumeration) as isize,
        )
    } == 0
    {
        return Err("EnumWindows failed".to_owned());
    }

    let mut paths = HashMap::new();
    Ok(context
        .windows
        .into_iter()
        .map(|window| {
            let executable_path = paths
                .entry(window.pid)
                .or_insert_with(|| process_path(window.pid))
                .clone();
            CurrentWindow {
                hwnd: window.hwnd,
                title: window.title,
                bounds: window.bounds,
                executable_path,
                monitor: window.monitor,
            }
        })
        .collect())
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
    let Some(monitor) = monitor_info(monitor) else {
        return 1;
    };

    let context = unsafe { &mut *(data as *mut WindowEnumeration) };
    context.windows.push(CurrentWindowRaw {
        hwnd: hwnd as usize,
        pid,
        title,
        bounds,
        monitor,
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
    (unsafe { GetWindowRect(hwnd, &mut rect) } != 0).then_some(rect.into())
}

fn monitor_info(monitor: Hmonitor) -> Option<MonitorInfo> {
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
    Some(MonitorInfo {
        device_name: wide_to_string(&info.device),
        work_area: info.work.into(),
    })
}

fn process_path(pid: u32) -> Option<String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut buffer = vec![0_u16; 32_768];
    let mut length = buffer.len() as u32;
    let value =
        if unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) } != 0
            && length > 0
        {
            Some(String::from_utf16_lossy(&buffer[..length as usize]))
        } else {
            None
        };
    unsafe {
        CloseHandle(process);
    }
    value
}

fn normalize_path(value: &str) -> String {
    let normalized = value.trim().replace('/', "\\");
    normalized
        .strip_prefix(r"\\?\")
        .unwrap_or(&normalized)
        .to_ascii_lowercase()
}

fn file_name(value: &str) -> Option<&str> {
    Path::new(value).file_name()?.to_str()
}

fn wide_to_string(buffer: &[u16]) -> String {
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str) -> SavedApplication {
        SavedApplication {
            name: name.to_owned(),
            executable_path: Some(format!(r"C:\Apps\{name}.exe")),
            app_user_model_id: None,
            file_version: None,
            classification: "user-application".to_owned(),
            launch: None,
            windows: Vec::new(),
            discovered_as_background: false,
        }
    }

    fn custom_window(title: &str, x: f64, width: f64) -> SavedWindow {
        SavedWindow {
            title: title.to_owned(),
            bounds: SavedRect {
                left: 0,
                top: 0,
                right: 100,
                bottom: 100,
            },
            restore_bounds: None,
            normalized_bounds: Some(SavedNormalizedRect {
                x,
                y: 0.0,
                width,
                height: 1.0,
            }),
            state: "snapped:custom".to_owned(),
            display_device: "DISPLAY1".to_owned(),
            display_relation: "primary".to_owned(),
            display_scale_percent: 100,
            is_foreground: false,
            z_order: 0,
            virtual_desktop_id: None,
            is_on_current_virtual_desktop: Some(true),
            taskbar_candidate: true,
        }
    }

    #[test]
    fn portrait_top_bottom_pair_is_deferred_as_one_unit() {
        let mut top_app = app("Top");
        let mut bottom_app = app("Bottom");
        let mut top = custom_window("Top", 0.0, 1.0);
        top.state = "snapped:top-half".to_owned();
        top.normalized_bounds = Some(stock_normalized(SnapSlot::TopHalf));
        top.bounds = SavedRect {
            left: 0,
            top: 0,
            right: 1080,
            bottom: 960,
        };
        let mut bottom = custom_window("Bottom", 0.0, 1.0);
        bottom.state = "snapped:bottom-half".to_owned();
        bottom.normalized_bounds = Some(stock_normalized(SnapSlot::BottomHalf));
        bottom.bounds = SavedRect {
            left: 0,
            top: 960,
            right: 1080,
            bottom: 1920,
        };
        top_app.windows.push(top);
        bottom_app.windows.push(bottom);
        let desktop = SavedDesktop {
            status: "available".to_owned(),
            displays: vec![SavedDisplay {
                device_name: "DISPLAY1".to_owned(),
                bounds: SavedRect {
                    left: 0,
                    top: 0,
                    right: 1080,
                    bottom: 1920,
                },
                work_area: SavedRect {
                    left: 0,
                    top: 0,
                    right: 1080,
                    bottom: 1920,
                },
                is_primary: true,
                scale_percent: 100,
                orientation: "portrait".to_owned(),
                relation_to_primary: "primary".to_owned(),
            }],
            applications: vec![top_app, bottom_app],
        };

        assert_eq!(portrait_stacked_pair_indices(&desktop).len(), 1);
        let placement = geometry_only_portrait_stacked_pairs(&desktop);
        assert_eq!(placement.applications[0].windows[0].state, "normal");
        assert_eq!(placement.applications[1].windows[0].state, "normal");
        assert_eq!(desktop.applications[0].windows[0].state, "snapped:top-half");
        assert_eq!(
            desktop.applications[1].windows[0].state,
            "snapped:bottom-half"
        );
    }

    #[test]
    fn landscape_top_bottom_pair_keeps_existing_restore_path() {
        let mut top_app = app("Top");
        let mut bottom_app = app("Bottom");
        let mut top = custom_window("Top", 0.0, 1.0);
        top.state = "snapped:top-half".to_owned();
        top.normalized_bounds = Some(stock_normalized(SnapSlot::TopHalf));
        let mut bottom = custom_window("Bottom", 0.0, 1.0);
        bottom.state = "snapped:bottom-half".to_owned();
        bottom.normalized_bounds = Some(stock_normalized(SnapSlot::BottomHalf));
        top_app.windows.push(top);
        bottom_app.windows.push(bottom);
        let desktop = SavedDesktop {
            status: "available".to_owned(),
            displays: vec![SavedDisplay {
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
                scale_percent: 100,
                orientation: "landscape".to_owned(),
                relation_to_primary: "primary".to_owned(),
            }],
            applications: vec![top_app, bottom_app],
        };

        assert!(portrait_stacked_pair_indices(&desktop).is_empty());
        let placement = geometry_only_portrait_stacked_pairs(&desktop);
        assert_eq!(
            placement.applications[0].windows[0].state,
            "snapped:top-half"
        );
        assert_eq!(
            placement.applications[1].windows[0].state,
            "snapped:bottom-half"
        );
    }

    #[test]
    fn arbitrary_side_by_side_ratio_forms_pair() {
        let left_app = app("Left");
        let right_app = app("Right");
        let left_window = custom_window("Left", 0.0, 0.217);
        let right_window = custom_window("Right", 0.217, 0.783);
        let left = SavedCustom {
            app: &left_app,
            window: &left_window,
            normalized: left_window.normalized_bounds.unwrap(),
        };
        let right = SavedCustom {
            app: &right_app,
            window: &right_window,
            normalized: right_window.normalized_bounds.unwrap(),
        };
        let pair = pair_shape(left, right).expect("custom pair");
        assert_eq!(pair.orientation, SplitOrientation::SideBySide);
        assert!((pair.divider_fraction - 0.217).abs() < 0.001);
    }

    #[test]
    fn floating_custom_rectangles_do_not_form_a_full_screen_pair() {
        let left_app = app("Left");
        let right_app = app("Right");
        let mut left_window = custom_window("Left", 0.1, 0.3);
        left_window.normalized_bounds.as_mut().unwrap().y = 0.1;
        left_window.normalized_bounds.as_mut().unwrap().height = 0.8;
        let right_window = custom_window("Right", 0.4, 0.3);
        let left = SavedCustom {
            app: &left_app,
            window: &left_window,
            normalized: left_window.normalized_bounds.unwrap(),
        };
        let right = SavedCustom {
            app: &right_app,
            window: &right_window,
            normalized: right_window.normalized_bounds.unwrap(),
        };
        assert!(pair_shape(left, right).is_none());
    }
}
