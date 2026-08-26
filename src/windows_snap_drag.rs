use std::{
    collections::HashSet,
    ffi::c_void,
    mem::{size_of, zeroed},
    thread,
    time::{Duration, Instant},
};

use crate::windows_snap_coord::{self, SnapDirection};

type Hwnd = *mut c_void;
type Hmonitor = *mut c_void;
type Bool = i32;

const INPUT_MOUSE: u32 = 0;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const WM_NCHITTEST: u32 = 0x0084;
const SMTO_ABORTIFHUNG: u32 = 0x0002;
const HTCAPTION: i32 = 2;
const MONITOR_DEFAULTTONEAREST: u32 = 2;
const HIT_TEST_TIMEOUT_MS: u32 = 120;
const FOCUS_TIMEOUT: Duration = Duration::from_millis(1_400);
const FOCUS_POLL: Duration = Duration::from_millis(20);
const DRAG_HOVER_SETTLE: Duration = Duration::from_millis(70);
const DRAG_STEP_SETTLE: Duration = Duration::from_millis(12);
const SNAP_SETTLE: Duration = Duration::from_millis(420);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
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

#[repr(C)]
#[derive(Clone, Copy)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KeyboardInput {
    virtual_key: u16,
    scan_code: u16,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct HardwareInput {
    message: u32,
    param_l: u16,
    param_h: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
union InputPayload {
    mouse: MouseInput,
    keyboard: KeyboardInput,
    hardware: HardwareInput,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NativeInput {
    kind: u32,
    payload: InputPayload,
}

struct CursorRestoreGuard {
    point: Option<Point>,
}

impl Drop for CursorRestoreGuard {
    fn drop(&mut self) {
        if let Some(point) = self.point {
            unsafe {
                SetCursorPos(point.x, point.y);
            }
        }
    }
}

struct InputQueueAttachment {
    current_thread: u32,
    attached_threads: Vec<u32>,
}

impl InputQueueAttachment {
    fn for_window(hwnd: Hwnd) -> Result<Self, String> {
        let current_thread = unsafe { GetCurrentThreadId() };
        let mut threads = Vec::new();

        let foreground = unsafe { GetForegroundWindow() };
        if !foreground.is_null() {
            let thread = unsafe { GetWindowThreadProcessId(foreground, std::ptr::null_mut()) };
            if thread != 0 {
                threads.push((thread, false));
            }
        }

        let target_thread = unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };
        if target_thread == 0 {
            return Err("could not resolve target window thread for snap drag".to_owned());
        }
        threads.push((target_thread, true));

        let mut seen = HashSet::new();
        let mut attached_threads = Vec::new();
        for (thread, required) in threads {
            if thread == current_thread || !seen.insert(thread) {
                continue;
            }
            if unsafe { AttachThreadInput(current_thread, thread, 1) } != 0 {
                attached_threads.push(thread);
            } else if required {
                for attached in attached_threads.iter().rev() {
                    unsafe {
                        AttachThreadInput(current_thread, *attached, 0);
                    }
                }
                return Err("AttachThreadInput failed for native snap drag".to_owned());
            }
        }

        Ok(Self {
            current_thread,
            attached_threads,
        })
    }
}

impl Drop for InputQueueAttachment {
    fn drop(&mut self) {
        for thread in self.attached_threads.iter().rev() {
            unsafe {
                AttachThreadInput(self.current_thread, *thread, 0);
            }
        }
    }
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetForegroundWindow() -> Hwnd;
    fn SetForegroundWindow(hwnd: Hwnd) -> Bool;
    fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
    fn AttachThreadInput(id_attach: u32, id_attach_to: u32, attach: Bool) -> Bool;
    fn GetWindowRect(hwnd: Hwnd, rect: *mut NativeRect) -> Bool;
    fn MonitorFromWindow(hwnd: Hwnd, flags: u32) -> Hmonitor;
    fn GetMonitorInfoW(monitor: Hmonitor, info: *mut MonitorInfo) -> Bool;
    fn GetCursorPos(point: *mut Point) -> Bool;
    fn SetCursorPos(x: i32, y: i32) -> Bool;
    fn SendInput(count: u32, inputs: *const NativeInput, size: i32) -> u32;
    fn SendMessageTimeoutW(
        hwnd: Hwnd,
        message: u32,
        wparam: usize,
        lparam: isize,
        flags: u32,
        timeout: u32,
        result: *mut usize,
    ) -> isize;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
}

pub(crate) fn snap_half_by_drag(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
    if hwnd.is_null() {
        return Err("window handle is unavailable".to_owned());
    }
    if !matches!(direction, SnapDirection::LeftHalf | SnapDirection::RightHalf) {
        return Ok(false);
    }
    if windows_snap_coord::is_arranged(hwnd).is_none() {
        return Err("IsWindowArranged is unavailable; cannot verify drag snap".to_owned());
    }

    let _attachment = InputQueueAttachment::for_window(hwnd)?;
    focus_without_geometry_change(hwnd)?;

    let rect = window_rect(hwnd).ok_or_else(|| "could not read target window bounds".to_owned())?;
    let caption = find_caption_point(hwnd, rect).ok_or_else(|| {
        "target window exposed no safe HTCAPTION point; refusing to drag application content"
            .to_owned()
    })?;
    let monitor = monitor_bounds(hwnd)
        .ok_or_else(|| "could not read physical monitor bounds for native snap".to_owned())?;

    let target = drag_target(monitor, caption, direction);
    drag_caption(caption, target)?;
    thread::sleep(SNAP_SETTLE);
    Ok(windows_snap_coord::is_arranged(hwnd) == Some(true))
}

fn focus_without_geometry_change(hwnd: Hwnd) -> Result<(), String> {
    if unsafe { GetForegroundWindow() } == hwnd {
        return Ok(());
    }
    unsafe {
        SetForegroundWindow(hwnd);
    }
    let deadline = Instant::now() + FOCUS_TIMEOUT;
    while Instant::now() < deadline {
        if unsafe { GetForegroundWindow() } == hwnd {
            return Ok(());
        }
        thread::sleep(FOCUS_POLL);
    }
    Err("Windows foreground-lock policy prevented focusing target window for snap drag".to_owned())
}

fn window_rect(hwnd: Hwnd) -> Option<NativeRect> {
    let mut rect = NativeRect::default();
    (unsafe { GetWindowRect(hwnd, &mut rect) } != 0
        && rect.right > rect.left
        && rect.bottom > rect.top)
        .then_some(rect)
}

fn monitor_bounds(hwnd: Hwnd) -> Option<NativeRect> {
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }
    let mut info: MonitorInfo = unsafe { zeroed() };
    info.size = size_of::<MonitorInfo>() as u32;
    (unsafe { GetMonitorInfoW(monitor, &mut info) } != 0).then_some(info.monitor)
}

fn find_caption_point(hwnd: Hwnd, rect: NativeRect) -> Option<(i32, i32)> {
    let width = rect.right.saturating_sub(rect.left);
    let height = rect.bottom.saturating_sub(rect.top);
    if width <= 0 || height <= 0 {
        return None;
    }

    let x_fractions = [0.50_f64, 0.35, 0.65, 0.20, 0.80, 0.10, 0.90];
    let max_depth = height.min(140);
    let y_offsets = [4_i32, 8, 12, 16, 20, 24, 28, 32, 38, 44, 52, 60, 72, 84, 96, 112, 128];

    for y_offset in y_offsets {
        if y_offset >= max_depth {
            break;
        }
        let y = rect.top.saturating_add(y_offset);
        for fraction in x_fractions {
            let x = rect.left + (width as f64 * fraction).round() as i32;
            if non_client_hit_test(hwnd, (x, y)) == Some(HTCAPTION) {
                return Some((x, y));
            }
        }
    }
    None
}

fn drag_target(monitor: NativeRect, caption: (i32, i32), direction: SnapDirection) -> (i32, i32) {
    let x = match direction {
        SnapDirection::LeftHalf => monitor.left,
        SnapDirection::RightHalf => monitor.right.saturating_sub(1),
        _ => caption.0,
    };
    let min_y = monitor.top.saturating_add(8);
    let max_y = monitor.bottom.saturating_sub(8).max(min_y);
    (x, caption.1.clamp(min_y, max_y))
}

fn non_client_hit_test(hwnd: Hwnd, point: (i32, i32)) -> Option<i32> {
    let lparam = screen_point_lparam(point.0, point.1)?;
    let mut result = 0_usize;
    let delivered = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_NCHITTEST,
            0,
            lparam,
            SMTO_ABORTIFHUNG,
            HIT_TEST_TIMEOUT_MS,
            &mut result,
        )
    };
    (delivered != 0).then_some(result as isize as i32)
}

fn screen_point_lparam(x: i32, y: i32) -> Option<isize> {
    if x < i16::MIN as i32 || x > i16::MAX as i32 || y < i16::MIN as i32 || y > i16::MAX as i32 {
        return None;
    }
    let low = x as i16 as u16 as u32;
    let high = y as i16 as u16 as u32;
    Some(((high << 16) | low) as u32 as isize)
}

fn drag_caption(start: (i32, i32), end: (i32, i32)) -> Result<(), String> {
    let mut original = Point::default();
    let _cursor_restore = CursorRestoreGuard {
        point: (unsafe { GetCursorPos(&mut original) } != 0).then_some(original),
    };

    if unsafe { SetCursorPos(start.0, start.1) } == 0 {
        return Err("SetCursorPos failed while targeting window caption".to_owned());
    }
    thread::sleep(DRAG_HOVER_SETTLE);
    send_mouse_button(true)?;

    let steps = 24_i64;
    for step in 1..=steps {
        let x = start.0 as i64 + ((end.0 as i64 - start.0 as i64) * step) / steps;
        let y = start.1 as i64 + ((end.1 as i64 - start.1 as i64) * step) / steps;
        if unsafe { SetCursorPos(x as i32, y as i32) } == 0 {
            let _ = send_mouse_button(false);
            return Err("SetCursorPos failed during native snap drag".to_owned());
        }
        thread::sleep(DRAG_STEP_SETTLE);
    }

    send_mouse_button(false)
}

fn mouse_input(flags: u32) -> NativeInput {
    NativeInput {
        kind: INPUT_MOUSE,
        payload: InputPayload {
            mouse: MouseInput {
                dx: 0,
                dy: 0,
                mouse_data: 0,
                flags,
                time: 0,
                extra_info: 0,
            },
        },
    }
}

fn send_mouse_button(down: bool) -> Result<(), String> {
    let input = mouse_input(if down {
        MOUSEEVENTF_LEFTDOWN
    } else {
        MOUSEEVENTF_LEFTUP
    });
    let sent = unsafe { SendInput(1, &input, size_of::<NativeInput>() as i32) };
    if sent == 1 {
        Ok(())
    } else {
        Err(format!(
            "Windows rejected synthetic mouse {} for native snap drag (possibly UIPI)",
            if down { "down" } else { "up" }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_target_uses_physical_monitor_edge_without_changing_vertical_caption_coordinate() {
        let monitor = NativeRect {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        assert_eq!(drag_target(monitor, (-900, 40), SnapDirection::LeftHalf), (-1920, 40));
        assert_eq!(drag_target(monitor, (-900, 40), SnapDirection::RightHalf), (-1, 40));
    }

    #[test]
    fn screen_point_pack_preserves_negative_monitor_coordinates() {
        let packed = screen_point_lparam(-120, 45).expect("packed") as u32;
        assert_eq!(packed as u16 as i16, -120);
        assert_eq!((packed >> 16) as u16 as i16, 45);
    }

    #[test]
    fn input_layout_matches_win32_abi() {
        let expected = if cfg!(target_pointer_width = "64") { 40 } else { 28 };
        assert_eq!(size_of::<NativeInput>(), expected);
    }
}
