use std::{
    cell::Cell,
    ffi::c_void,
    mem::size_of,
    thread,
    time::Duration,
};

use crate::windows_snap_core;

pub(crate) use windows_snap_core::{
    SnapDirection, SplitOrientation, is_arranged, take_last_arrangement_check,
};

type Hwnd = *mut c_void;
type Hmonitor = *mut c_void;
type Bool = i32;

const SW_RESTORE: i32 = 9;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;
const SWP_NOOWNERZORDER: u32 = 0x0200;
const MONITOR_DEFAULTTONEAREST: u32 = 2;
const RESTORE_SETTLE: Duration = Duration::from_millis(90);
const BASELINE_SETTLE: Duration = Duration::from_millis(110);
const WORK_AREA_TOLERANCE: i32 = 3;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct NativeRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct MonitorInfo {
    size: u32,
    monitor: NativeRect,
    work: NativeRect,
    flags: u32,
}

#[link(name = "user32")]
unsafe extern "system" {
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
    fn MonitorFromWindow(hwnd: Hwnd, flags: u32) -> Hmonitor;
    fn GetMonitorInfoW(monitor: Hmonitor, info: *mut MonitorInfo) -> Bool;
    fn GetWindowRect(hwnd: Hwnd, rect: *mut NativeRect) -> Bool;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLastError() -> u32;
}

thread_local! {
    /// Custom native-Snap restore knows the exact monitor work area captured from
    /// the live window inventory. Keeping that target here means every retry can
    /// reset an accidentally displaced HWND back to the intended monitor rather
    /// than trusting whichever monitor Windows left it on.
    static TARGET_WORK_AREA: Cell<Option<[i32; 4]>> = const { Cell::new(None) };
}

struct TargetWorkAreaGuard {
    previous: Option<[i32; 4]>,
}

impl Drop for TargetWorkAreaGuard {
    fn drop(&mut self) {
        TARGET_WORK_AREA.with(|target| target.set(self.previous));
    }
}

pub(crate) fn with_target_work_area<T>(
    work_area: [i32; 4],
    action: impl FnOnce() -> T,
) -> T {
    let previous = TARGET_WORK_AREA.with(|target| target.replace(Some(work_area)));
    let _guard = TargetWorkAreaGuard { previous };
    action()
}

/// Makes Win+Arrow deterministic before delegating to the existing native Snap
/// primitive.
///
/// Win+Arrow is stateful: when an HWND is already arranged, another arrow can
/// advance it to a different Snap slot or even an adjacent monitor. Microsoft
/// documents SW_RESTORE as the operation that restores an arranged window to
/// its normal state. We therefore restore, place a small floating rectangle well
/// inside the intended monitor, verify that it is no longer arranged and is on
/// that exact monitor, and only then permit the native shortcut.
pub(crate) fn snap(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
    prepare_floating_baseline(hwnd)?;
    windows_snap_core::snap(hwnd, direction)
}

fn prepare_floating_baseline(hwnd: Hwnd) -> Result<(), String> {
    if hwnd.is_null() {
        return Err("window handle is unavailable".to_owned());
    }

    let work_area = target_work_area(hwnd)?;
    let baseline = floating_baseline_rect(work_area).ok_or_else(|| {
        format!(
            "native-snap target work area {:?} cannot produce a safe floating baseline",
            work_area
        )
    })?;

    // SW_RESTORE is specifically documented to restore an arranged/snapped
    // window to its normal state. Its return value only reports prior visibility,
    // not success, so verification below is authoritative.
    unsafe {
        ShowWindow(hwnd, SW_RESTORE);
    }
    thread::sleep(RESTORE_SETTLE);

    let width = baseline.right - baseline.left;
    let height = baseline.bottom - baseline.top;
    let moved = unsafe {
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            baseline.left,
            baseline.top,
            width,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        )
    };
    if moved == 0 {
        let error = unsafe { GetLastError() };
        return Err(format!(
            "SetWindowPos failed while establishing the floating native-snap baseline (Win32 error {error})"
        ));
    }
    thread::sleep(BASELINE_SETTLE);

    match is_arranged(hwnd) {
        Some(false) => {}
        Some(true) => {
            return Err(
                "SW_RESTORE/SetWindowPos did not clear the window's arranged state; refusing stateful Win+Arrow"
                    .to_owned(),
            );
        }
        None => {
            return Err(
                "Windows does not expose IsWindowArranged; cannot verify the floating baseline before native snap"
                    .to_owned(),
            );
        }
    }

    let actual_work = monitor_work_area(hwnd).ok_or_else(|| {
        "could not determine the target window's monitor after establishing its floating baseline"
            .to_owned()
    })?;
    if !same_work_area(actual_work, work_area) {
        return Err(format!(
            "floating native-snap baseline landed on the wrong monitor: expected work area {:?}, got {:?}",
            work_area, actual_work
        ));
    }

    let mut actual = NativeRect::default();
    if unsafe { GetWindowRect(hwnd, &mut actual) } == 0 {
        return Err("could not verify floating native-snap window bounds".to_owned());
    }
    if !rect_center_inside(actual, work_area) {
        return Err(format!(
            "floating native-snap baseline is not centered inside the intended monitor: bounds [{}, {}, {}, {}], work area {:?}",
            actual.left, actual.top, actual.right, actual.bottom, work_area
        ));
    }

    Ok(())
}

fn target_work_area(hwnd: Hwnd) -> Result<[i32; 4], String> {
    if let Some(target) = TARGET_WORK_AREA.with(Cell::get) {
        validate_work_area(target)?;
        return Ok(target);
    }

    let work = monitor_work_area(hwnd)
        .ok_or_else(|| "could not determine the current monitor for native snap".to_owned())?;
    validate_work_area(work)?;
    Ok(work)
}

fn validate_work_area(area: [i32; 4]) -> Result<(), String> {
    if area[2] <= area[0] || area[3] <= area[1] {
        return Err(format!("invalid native-snap monitor work area {area:?}"));
    }
    Ok(())
}

fn monitor_work_area(hwnd: Hwnd) -> Option<[i32; 4]> {
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }

    let mut info = MonitorInfo {
        size: size_of::<MonitorInfo>() as u32,
        monitor: NativeRect::default(),
        work: NativeRect::default(),
        flags: 0,
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }

    Some([
        info.work.left,
        info.work.top,
        info.work.right,
        info.work.bottom,
    ])
}

fn floating_baseline_rect(area: [i32; 4]) -> Option<NativeRect> {
    let width = area[2].checked_sub(area[0])?;
    let height = area[3].checked_sub(area[1])?;
    if width <= 0 || height <= 0 {
        return None;
    }

    // Roughly 60% of each dimension, leaving enough margin that Windows cannot
    // interpret the placement itself as an edge/corner Snap gesture. The inset
    // calculation also behaves correctly for negative monitor coordinates.
    let inset_x = ((width / 5).max(32)).min((width / 3).max(1));
    let inset_y = ((height / 5).max(32)).min((height / 3).max(1));
    let left = area[0].saturating_add(inset_x);
    let top = area[1].saturating_add(inset_y);
    let right = area[2].saturating_sub(inset_x);
    let bottom = area[3].saturating_sub(inset_y);

    (right > left && bottom > top).then_some(NativeRect {
        left,
        top,
        right,
        bottom,
    })
}

fn same_work_area(actual: [i32; 4], expected: [i32; 4]) -> bool {
    actual
        .iter()
        .zip(expected.iter())
        .all(|(actual, expected)| actual.saturating_sub(*expected).abs() <= WORK_AREA_TOLERANCE)
}

fn rect_center_inside(rect: NativeRect, area: [i32; 4]) -> bool {
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return false;
    }
    let center_x = (rect.left as i64 + rect.right as i64) / 2;
    let center_y = (rect.top as i64 + rect.bottom as i64) / 2;
    center_x >= area[0] as i64
        && center_x < area[2] as i64
        && center_y >= area[1] as i64
        && center_y < area[3] as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floating_baseline_stays_inside_positive_monitor() {
        let rect = floating_baseline_rect([0, 0, 1920, 1040]).expect("baseline");
        assert!(rect.left > 0);
        assert!(rect.top > 0);
        assert!(rect.right < 1920);
        assert!(rect.bottom < 1040);
        assert!(rect_center_inside(rect, [0, 0, 1920, 1040]));
    }

    #[test]
    fn floating_baseline_stays_inside_negative_monitor() {
        let area = [-1920, -120, 0, 960];
        let rect = floating_baseline_rect(area).expect("baseline");
        assert!(rect.left > area[0]);
        assert!(rect.top > area[1]);
        assert!(rect.right < area[2]);
        assert!(rect.bottom < area[3]);
        assert!(rect_center_inside(rect, area));
    }

    #[test]
    fn work_area_comparison_allows_tiny_shell_variance() {
        assert!(same_work_area([0, 0, 1920, 1040], [1, -1, 1919, 1042]));
        assert!(!same_work_area([1920, 0, 3840, 1040], [0, 0, 1920, 1040]));
    }

    #[test]
    fn target_work_area_scope_restores_previous_value() {
        TARGET_WORK_AREA.with(|target| target.set(Some([1, 2, 3, 4])));
        with_target_work_area([10, 20, 30, 40], || {
            assert_eq!(TARGET_WORK_AREA.with(Cell::get), Some([10, 20, 30, 40]));
        });
        assert_eq!(TARGET_WORK_AREA.with(Cell::get), Some([1, 2, 3, 4]));
        TARGET_WORK_AREA.with(|target| target.set(None));
    }
}
