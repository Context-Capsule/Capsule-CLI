use std::{
    collections::HashSet,
    ffi::c_void,
    mem::size_of,
    thread,
    time::Duration,
};

use crate::windows_snap_legacy;

pub(crate) use windows_snap_legacy::{
    SnapDirection, SplitOrientation, is_arranged, take_last_arrangement_check,
};

type Hwnd = *mut c_void;
type Bool = i32;

const INPUT_MOUSE: u32 = 0;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const WM_NCHITTEST: u32 = 0x0084;
const SMTO_ABORTIFHUNG: u32 = 0x0002;
const HTCAPTION: i32 = 2;
const HTLEFT: i32 = 10;
const HTRIGHT: i32 = 11;
const HTTOP: i32 = 12;
const HTTOPLEFT: i32 = 13;
const HTTOPRIGHT: i32 = 14;
const HTBOTTOM: i32 = 15;
const HTBOTTOMLEFT: i32 = 16;
const HTBOTTOMRIGHT: i32 = 17;
const ACTIVATION_SETTLE: Duration = Duration::from_millis(70);
const HIT_TEST_TIMEOUT_MS: u32 = 120;

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

#[link(name = "user32")]
unsafe extern "system" {
    fn GetForegroundWindow() -> Hwnd;
    fn SetForegroundWindow(hwnd: Hwnd) -> Bool;
    fn BringWindowToTop(hwnd: Hwnd) -> Bool;
    fn SetActiveWindow(hwnd: Hwnd) -> Hwnd;
    fn SetFocus(hwnd: Hwnd) -> Hwnd;
    fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
    fn AttachThreadInput(id_attach: u32, id_attach_to: u32, attach: Bool) -> Bool;
    fn GetWindowRect(hwnd: Hwnd, rect: *mut NativeRect) -> Bool;
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
    fn GetLastError() -> u32;
}

/// Keeps the CLI thread, the current foreground thread, and every target GUI
/// thread on one input queue for the duration of a native-Snap operation.
///
/// AttachThreadInput does not itself grant foreground privilege. It is only the
/// coordination prerequisite that lets activation/focus calls operate across
/// the participating GUI threads. The target is still verified with
/// GetForegroundWindow before the legacy snap routine is allowed to send keys.
struct InputQueueAttachment {
    current_thread: u32,
    attached_threads: Vec<u32>,
}

impl InputQueueAttachment {
    fn for_windows(targets: &[usize]) -> Result<Self, String> {
        let current_thread = unsafe { GetCurrentThreadId() };
        let mut candidates = Vec::new();

        let foreground = unsafe { GetForegroundWindow() };
        if !foreground.is_null() {
            let thread = unsafe { GetWindowThreadProcessId(foreground, std::ptr::null_mut()) };
            if thread != 0 {
                candidates.push((thread, false));
            }
        }

        for target in targets {
            let hwnd = *target as Hwnd;
            if hwnd.is_null() {
                continue;
            }
            let thread = unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };
            if thread == 0 {
                return Err("could not resolve a target window thread for native snap".to_owned());
            }
            candidates.push((thread, true));
        }

        let mut seen = HashSet::new();
        let mut attached_threads = Vec::new();
        for (thread, required) in candidates {
            if thread == current_thread || !seen.insert(thread) {
                continue;
            }
            if unsafe { AttachThreadInput(current_thread, thread, 1) } != 0 {
                attached_threads.push(thread);
            } else if required {
                let error = unsafe { GetLastError() };
                for attached in attached_threads.iter().rev() {
                    unsafe {
                        AttachThreadInput(current_thread, *attached, 0);
                    }
                }
                return Err(format!(
                    "AttachThreadInput failed for a native-snap target thread (Win32 error {error})"
                ));
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

fn activate_target(hwnd: Hwnd) -> Result<(), String> {
    if hwnd.is_null() {
        return Err("window handle is unavailable".to_owned());
    }
    if unsafe { GetForegroundWindow() } == hwnd {
        return Ok(());
    }

    // First use the non-invasive activation APIs while the target GUI thread is
    // attached to our input queue. BringWindowToTop activates a top-level window;
    // SetActiveWindow/SetFocus are valid across attached queues.
    unsafe {
        BringWindowToTop(hwnd);
        SetActiveWindow(hwnd);
        SetFocus(hwnd);
        SetForegroundWindow(hwnd);
    }
    thread::sleep(ACTIVATION_SETTLE);
    if unsafe { GetForegroundWindow() } == hwnd {
        return Ok(());
    }

    // Windows may still enforce foreground-lock policy even after the normal
    // activation conditions are satisfied. A real click is the user-level way
    // Windows defines foreground activation. To avoid changing application
    // content, click only a point that the target itself reports as non-client
    // chrome (caption or resize frame), then verify the exact HWND again.
    let point = safe_non_client_activation_point(hwnd).ok_or_else(|| {
        "Windows refused programmatic foreground activation and no safe non-client activation point was found"
            .to_owned()
    })?;
    click_non_client_point(point)?;
    thread::sleep(ACTIVATION_SETTLE);

    unsafe {
        BringWindowToTop(hwnd);
        SetActiveWindow(hwnd);
        SetFocus(hwnd);
        SetForegroundWindow(hwnd);
    }
    thread::sleep(ACTIVATION_SETTLE);

    if unsafe { GetForegroundWindow() } == hwnd {
        Ok(())
    } else {
        Err(
            "the intended window still did not become foreground after verified non-client activation; native snap shortcut was not sent"
                .to_owned(),
        )
    }
}

fn safe_non_client_activation_point(hwnd: Hwnd) -> Option<(i32, i32)> {
    let mut rect = NativeRect::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0
        || rect.right <= rect.left
        || rect.bottom <= rect.top
    {
        return None;
    }

    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    let fractions = [0.50_f64, 0.25, 0.75, 0.38, 0.62];
    let offsets = [0_i32, 1, 2, 3, 4, 5, 6, 8, 10, 12];

    for fraction in fractions {
        let x = rect.left + (width as f64 * fraction).round() as i32;
        let y = rect.top + (height as f64 * fraction).round() as i32;
        for offset in offsets {
            let candidates = [
                (x, rect.top.saturating_add(offset)),
                (x, rect.bottom.saturating_sub(1 + offset)),
                (rect.left.saturating_add(offset), y),
                (rect.right.saturating_sub(1 + offset), y),
            ];
            for point in candidates {
                if non_client_hit_test(hwnd, point).is_some_and(is_safe_activation_hit) {
                    return Some(point);
                }
            }
        }
    }

    None
}

fn is_safe_activation_hit(hit: i32) -> bool {
    matches!(
        hit,
        HTCAPTION
            | HTLEFT
            | HTRIGHT
            | HTTOP
            | HTTOPLEFT
            | HTTOPRIGHT
            | HTBOTTOM
            | HTBOTTOMLEFT
            | HTBOTTOMRIGHT
    )
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
    if x < i16::MIN as i32
        || x > i16::MAX as i32
        || y < i16::MIN as i32
        || y > i16::MAX as i32
    {
        return None;
    }
    let low = x as i16 as u16 as u32;
    let high = y as i16 as u16 as u32;
    Some(((high << 16) | low) as u32 as isize)
}

fn click_non_client_point(point: (i32, i32)) -> Result<(), String> {
    let mut original = Point::default();
    let _cursor_restore = CursorRestoreGuard {
        point: (unsafe { GetCursorPos(&mut original) } != 0).then_some(original),
    };

    if unsafe { SetCursorPos(point.0, point.1) } == 0 {
        return Err("SetCursorPos failed while activating the native-snap target".to_owned());
    }
    thread::sleep(Duration::from_millis(35));

    let inputs = [
        mouse_input(MOUSEEVENTF_LEFTDOWN),
        mouse_input(MOUSEEVENTF_LEFTUP),
    ];
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<NativeInput>() as i32,
        )
    };
    if sent == inputs.len() as u32 {
        Ok(())
    } else {
        Err(format!(
            "Windows accepted {sent}/{} non-client activation mouse events (possibly blocked by UIPI)",
            inputs.len()
        ))
    }
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

pub(crate) fn snap(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
    let _attachment = InputQueueAttachment::for_windows(&[hwnd as usize])?;
    activate_target(hwnd)?;

    // The legacy routine still performs its own exact GetForegroundWindow check
    // before injecting Win+Arrow and verifies IsWindowArranged afterward.
    windows_snap_legacy::snap(hwnd, direction)
}

fn prime_pair_foreground_queue(hwnd: Hwnd) -> Result<(), String> {
    if hwnd.is_null() {
        return Err("window handle is unavailable".to_owned());
    }

    // The custom-pair routine changes foreground HWND more than once *inside*
    // the legacy implementation. Merely making the first HWND foreground is not
    // enough on Windows sessions with strict foreground locking. Generate one
    // verified non-client click after all participating GUI threads are attached
    // so the shared input queue receives genuine input before those transitions.
    unsafe {
        BringWindowToTop(hwnd);
    }
    let point = safe_non_client_activation_point(hwnd).ok_or_else(|| {
        "no safe non-client activation point was found for the custom-snap foreground handoff"
            .to_owned()
    })?;
    click_non_client_point(point)?;
    thread::sleep(ACTIVATION_SETTLE);

    unsafe {
        BringWindowToTop(hwnd);
        SetActiveWindow(hwnd);
        SetFocus(hwnd);
        SetForegroundWindow(hwnd);
    }
    thread::sleep(ACTIVATION_SETTLE);

    if unsafe { GetForegroundWindow() } == hwnd {
        Ok(())
    } else {
        Err(
            "custom-snap target did not become foreground after verified non-client input; native snap shortcuts were not sent"
                .to_owned(),
        )
    }
}

pub(crate) fn restore_resized_pair(
    first_hwnd: usize,
    second_hwnd: usize,
    orientation: SplitOrientation,
    work_area: [i32; 4],
    divider_fraction: f64,
) -> Result<(), String> {
    let _attachment = InputQueueAttachment::for_windows(&[first_hwnd, second_hwnd])?;

    prime_pair_foreground_queue(first_hwnd as Hwnd)?;

    windows_snap_legacy::restore_resized_pair(
        first_hwnd,
        second_hwnd,
        orientation,
        work_area,
        divider_fraction,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_preserves_snap_direction_type() {
        let direction = SnapDirection::LeftHalf;
        assert_eq!(direction, SnapDirection::LeftHalf);
    }

    #[test]
    fn wrapper_preserves_split_orientation_type() {
        let orientation = SplitOrientation::SideBySide;
        assert_eq!(orientation, SplitOrientation::SideBySide);
    }

    #[test]
    fn activation_hits_exclude_client_and_window_buttons() {
        assert!(is_safe_activation_hit(HTCAPTION));
        assert!(is_safe_activation_hit(HTLEFT));
        assert!(is_safe_activation_hit(HTBOTTOMRIGHT));
        assert!(!is_safe_activation_hit(1)); // HTCLIENT
        assert!(!is_safe_activation_hit(8)); // HTMINBUTTON
        assert!(!is_safe_activation_hit(9)); // HTMAXBUTTON
        assert!(!is_safe_activation_hit(20)); // HTCLOSE
    }

    #[test]
    fn screen_coordinates_preserve_negative_monitor_positions() {
        let packed = screen_point_lparam(-120, -30).expect("packed coordinate") as u32;
        assert_eq!(packed as u16 as i16, -120);
        assert_eq!((packed >> 16) as u16 as i16, -30);
    }

    #[test]
    fn native_input_layout_matches_win32_abi() {
        let expected = if cfg!(target_pointer_width = "64") { 40 } else { 28 };
        assert_eq!(size_of::<NativeInput>(), expected);
    }
}
