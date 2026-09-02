use std::{collections::HashSet, ffi::c_void, mem::size_of, thread, time::Duration};

use crate::windows_snap_legacy;

pub(crate) use windows_snap_legacy::{
    SnapDirection, SplitOrientation, is_arranged, take_last_arrangement_check,
};

type Hwnd = *mut c_void;
type Bool = i32;
type Hresult = i32;

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
const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;
const SNAP_SETTLE: Duration = Duration::from_millis(220);
const DIVIDER_HOVER_SETTLE: Duration = Duration::from_millis(90);
const DIVIDER_STEP_SETTLE: Duration = Duration::from_millis(12);
const DIVIDER_RESULT_SETTLE: Duration = Duration::from_millis(280);
const CUSTOM_TARGET_TOLERANCE: i32 = 24;
const EDGE_SCAN_OFFSETS: [i32; 21] = [
    0, -1, 1, -2, 2, -3, 3, -4, 4, -5, 5, -6, 6, -8, 8, -10, 10, -12, 12, -16, 16,
];

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

#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmGetWindowAttribute(
        hwnd: Hwnd,
        attribute: u32,
        value: *mut c_void,
        value_size: u32,
    ) -> Hresult;
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
            if thread == current_thread || seen.contains(&thread) {
                continue;
            }
            if unsafe { AttachThreadInput(current_thread, thread, 1) } != 0 {
                seen.insert(thread);
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
    if x < i16::MIN as i32 || x > i16::MAX as i32 || y < i16::MIN as i32 || y > i16::MAX as i32 {
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
    windows_snap_legacy::snap(hwnd, direction)
}

fn snap_once(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
    let _attachment = InputQueueAttachment::for_windows(&[hwnd as usize])?;
    activate_target(hwnd)?;
    windows_snap_legacy::snap_once(hwnd, direction)
}

fn equal_pair_member_matches_work_area(
    hwnd: Hwnd,
    orientation: SplitOrientation,
    first: bool,
    work_area: [i32; 4],
    tolerance: i32,
) -> bool {
    if is_arranged(hwnd) != Some(true) {
        return false;
    }
    let Some(rect) = frame_bounds(hwnd) else {
        return false;
    };
    let close = |actual: i32, expected: i32| (actual - expected).abs() <= tolerance;
    match (orientation, first) {
        (SplitOrientation::SideBySide, true) => {
            let divider = work_area[0] + (work_area[2] - work_area[0]) / 2;
            close(rect.left, work_area[0])
                && close(rect.top, work_area[1])
                && close(rect.right, divider)
                && close(rect.bottom, work_area[3])
        }
        (SplitOrientation::SideBySide, false) => {
            let divider = work_area[0] + (work_area[2] - work_area[0]) / 2;
            close(rect.left, divider)
                && close(rect.top, work_area[1])
                && close(rect.right, work_area[2])
                && close(rect.bottom, work_area[3])
        }
        (SplitOrientation::Stacked, true) => {
            let divider = work_area[1] + (work_area[3] - work_area[1]) / 2;
            close(rect.left, work_area[0])
                && close(rect.top, work_area[1])
                && close(rect.right, work_area[2])
                && close(rect.bottom, divider)
        }
        (SplitOrientation::Stacked, false) => {
            let divider = work_area[1] + (work_area[3] - work_area[1]) / 2;
            close(rect.left, work_area[0])
                && close(rect.top, divider)
                && close(rect.right, work_area[2])
                && close(rect.bottom, work_area[3])
        }
    }
}

fn establish_pair_once(
    first: Hwnd,
    second: Hwnd,
    orientation: SplitOrientation,
    work_area: [i32; 4],
) -> Result<(), String> {
    let (first_direction, second_direction) = match orientation {
        SplitOrientation::SideBySide => (SnapDirection::LeftHalf, SnapDirection::RightHalf),
        SplitOrientation::Stacked => (SnapDirection::TopHalf, SnapDirection::BottomHalf),
    };

    if !snap_once(first, first_direction)? {
        return Err(
            "Windows did not arrange the first window while creating the one-shot stock snap pair"
                .to_owned(),
        );
    }

    // IsWindowArranged alone is insufficient: on a tall portrait display the
    // shell may choose a top third. If that happened, stop immediately. Sending
    // the second shortcut cannot turn the first window into the saved half and
    // only creates more visible churn.
    if !equal_pair_member_matches_work_area(first, orientation, true, work_area, 3) {
        let observed = frame_bounds(first)
            .map(|rect| {
                format!(
                    "[{}, {}, {}, {}]",
                    rect.left, rect.top, rect.right, rect.bottom
                )
            })
            .unwrap_or_else(|| "unavailable".to_owned());
        return Err(format!(
            "the first native Snap shortcut entered an arranged zone {observed}, but not the saved 50/50 half; the second shortcut was intentionally not attempted"
        ));
    }

    if !snap_once(second, second_direction)? {
        return Err(
            "Windows did not arrange the second window while creating the one-shot stock snap pair"
                .to_owned(),
        );
    }
    thread::sleep(SNAP_SETTLE);

    if !equal_pair_member_matches_work_area(second, orientation, false, work_area, 3) {
        return Err(
            "the second native Snap shortcut did not land in the saved 50/50 half".to_owned(),
        );
    }
    Ok(())
}

fn establish_pair(first: Hwnd, second: Hwnd, orientation: SplitOrientation) -> Result<(), String> {
    let (first_direction, second_direction) = match orientation {
        SplitOrientation::SideBySide => (SnapDirection::LeftHalf, SnapDirection::RightHalf),
        SplitOrientation::Stacked => (SnapDirection::TopHalf, SnapDirection::BottomHalf),
    };

    if !snap(first, first_direction)? {
        return Err(
            "Windows did not arrange the first window while creating the custom snap pair"
                .to_owned(),
        );
    }
    if !snap(second, second_direction)? {
        return Err(
            "Windows did not arrange the second window while creating the custom snap pair"
                .to_owned(),
        );
    }
    thread::sleep(SNAP_SETTLE);

    if is_arranged(first) != Some(true) || is_arranged(second) != Some(true) {
        return Err(
            "one of the windows left arranged state while creating the custom snap pair".to_owned(),
        );
    }
    Ok(())
}

pub(crate) fn restore_equal_pair(
    first_hwnd: usize,
    second_hwnd: usize,
    orientation: SplitOrientation,
    work_area: [i32; 4],
) -> Result<(), String> {
    let first = first_hwnd as Hwnd;
    let second = second_hwnd as Hwnd;
    if first.is_null() || second.is_null() || first == second {
        return Err("stock snap pair has invalid window handles".to_owned());
    }

    let width = work_area[2].saturating_sub(work_area[0]);
    let height = work_area[3].saturating_sub(work_area[1]);
    if width <= 0 || height <= 0 {
        return Err("stock snap pair has an invalid monitor work area".to_owned());
    }

    // Exactly one TopHalf + one BottomHalf (or LeftHalf + RightHalf) attempt.
    // There is deliberately no divider drag and no retry loop here: if the two
    // verified native shortcuts do not settle, return the failure immediately.
    establish_pair_once(first, second, orientation, work_area)?;

    if equal_pair_matches_work_area(first, second, orientation, work_area, 3) {
        Ok(())
    } else {
        let target = match orientation {
            SplitOrientation::SideBySide => work_area[0] + width / 2,
            SplitOrientation::Stacked => work_area[1] + height / 2,
        };
        Err(format!(
            "Windows created the stock snap pair, but it did not settle into the expected 50/50 work-area halves: {}",
            pair_mismatch_description(first, second, orientation, target)
        ))
    }
}

fn equal_pair_matches_work_area(
    first: Hwnd,
    second: Hwnd,
    orientation: SplitOrientation,
    work_area: [i32; 4],
    tolerance: i32,
) -> bool {
    if is_arranged(first) != Some(true) || is_arranged(second) != Some(true) {
        return false;
    }
    let Some(first_rect) = frame_bounds(first) else {
        return false;
    };
    let Some(second_rect) = frame_bounds(second) else {
        return false;
    };
    let close = |actual: i32, expected: i32| (actual - expected).abs() <= tolerance;

    match orientation {
        SplitOrientation::SideBySide => {
            let divider = work_area[0] + (work_area[2] - work_area[0]) / 2;
            close(first_rect.left, work_area[0])
                && close(first_rect.top, work_area[1])
                && close(first_rect.right, divider)
                && close(first_rect.bottom, work_area[3])
                && close(second_rect.left, divider)
                && close(second_rect.top, work_area[1])
                && close(second_rect.right, work_area[2])
                && close(second_rect.bottom, work_area[3])
        }
        SplitOrientation::Stacked => {
            let divider = work_area[1] + (work_area[3] - work_area[1]) / 2;
            close(first_rect.left, work_area[0])
                && close(first_rect.top, work_area[1])
                && close(first_rect.right, work_area[2])
                && close(first_rect.bottom, divider)
                && close(second_rect.left, work_area[0])
                && close(second_rect.top, divider)
                && close(second_rect.right, work_area[2])
                && close(second_rect.bottom, work_area[3])
        }
    }
}

fn resize_hit_code(first_side: bool, orientation: SplitOrientation) -> i32 {
    match (orientation, first_side) {
        (SplitOrientation::SideBySide, true) => HTRIGHT,
        (SplitOrientation::SideBySide, false) => HTLEFT,
        (SplitOrientation::Stacked, true) => HTBOTTOM,
        (SplitOrientation::Stacked, false) => HTTOP,
    }
}

fn hit_name(hit: i32) -> &'static str {
    match hit {
        HTLEFT => "HTLEFT",
        HTRIGHT => "HTRIGHT",
        HTTOP => "HTTOP",
        HTBOTTOM => "HTBOTTOM",
        _ => "resize",
    }
}

fn find_resize_handle(
    hwnd: Hwnd,
    rect: NativeRect,
    first_side: bool,
    orientation: SplitOrientation,
) -> Option<(i32, i32)> {
    let expected = resize_hit_code(first_side, orientation);
    let cross_positions = [0.50_f64, 0.38, 0.62, 0.28, 0.72];

    for cross in cross_positions {
        let base = match orientation {
            SplitOrientation::SideBySide => {
                let y = rect.top
                    + ((rect.bottom.saturating_sub(rect.top)) as f64 * cross).round() as i32;
                let x = if first_side { rect.right } else { rect.left };
                (x, y)
            }
            SplitOrientation::Stacked => {
                let x = rect.left
                    + ((rect.right.saturating_sub(rect.left)) as f64 * cross).round() as i32;
                let y = if first_side { rect.bottom } else { rect.top };
                (x, y)
            }
        };

        for offset in EDGE_SCAN_OFFSETS {
            let point = match orientation {
                SplitOrientation::SideBySide => (base.0.saturating_add(offset), base.1),
                SplitOrientation::Stacked => (base.0, base.1.saturating_add(offset)),
            };
            if non_client_hit_test(hwnd, point) == Some(expected) {
                return Some(point);
            }
        }
    }

    None
}

fn drag_saved_edge(
    hwnd: Hwnd,
    first_side: bool,
    orientation: SplitOrientation,
    target: i32,
) -> Result<(), String> {
    let rect =
        frame_bounds(hwnd).ok_or_else(|| "could not read the arranged window frame".to_owned())?;
    let expected_hit = resize_hit_code(first_side, orientation);
    let start = find_resize_handle(hwnd, rect, first_side, orientation).ok_or_else(|| {
        format!(
            "Windows did not expose the expected {} resize handle around the arranged frame",
            hit_name(expected_hit)
        )
    })?;
    let end = match orientation {
        SplitOrientation::SideBySide => (target, start.1),
        SplitOrientation::Stacked => (start.0, target),
    };
    drag_resize_handle(hwnd, expected_hit, start, end)
}

fn drag_resize_handle(
    hwnd: Hwnd,
    expected_hit: i32,
    start: (i32, i32),
    end: (i32, i32),
) -> Result<(), String> {
    let _attachment = InputQueueAttachment::for_windows(&[hwnd as usize])?;
    activate_target(hwnd)?;

    if non_client_hit_test(hwnd, start) != Some(expected_hit) {
        return Err(format!(
            "the selected resize point no longer reports {} before the drag",
            hit_name(expected_hit)
        ));
    }

    drag_divider(start, end)
}

fn shared_divider_fallback(
    first: Hwnd,
    second: Hwnd,
    work_area: [i32; 4],
    orientation: SplitOrientation,
) -> Option<(i32, i32)> {
    let first_rect = frame_bounds(first)?;
    let second_rect = frame_bounds(second)?;
    Some(match orientation {
        SplitOrientation::SideBySide => {
            let x = ((first_rect.right as i64 + second_rect.left as i64) / 2) as i32;
            let y = work_area[1] + work_area[3].saturating_sub(work_area[1]) / 2;
            (x, y)
        }
        SplitOrientation::Stacked => {
            let y = ((first_rect.bottom as i64 + second_rect.top as i64) / 2) as i32;
            let x = work_area[0] + work_area[2].saturating_sub(work_area[0]) / 2;
            (x, y)
        }
    })
}

fn drag_divider(start: (i32, i32), end: (i32, i32)) -> Result<(), String> {
    let mut original = Point::default();
    let _cursor_restore = CursorRestoreGuard {
        point: (unsafe { GetCursorPos(&mut original) } != 0).then_some(original),
    };

    if unsafe { SetCursorPos(start.0, start.1) } == 0 {
        return Err(
            "SetCursorPos failed while targeting the Windows snap resize handle".to_owned(),
        );
    }
    thread::sleep(DIVIDER_HOVER_SETTLE);

    send_mouse_button(true)?;

    let steps = 18_i32;
    for step in 1..=steps {
        let x = start.0 as i64 + ((end.0 as i64 - start.0 as i64) * step as i64) / steps as i64;
        let y = start.1 as i64 + ((end.1 as i64 - start.1 as i64) * step as i64) / steps as i64;
        if unsafe { SetCursorPos(x as i32, y as i32) } == 0 {
            let _ = send_mouse_button(false);
            return Err("SetCursorPos failed during the Windows snap resize drag".to_owned());
        }
        thread::sleep(DIVIDER_STEP_SETTLE);
    }

    let release_result = send_mouse_button(false);
    thread::sleep(DIVIDER_RESULT_SETTLE);
    release_result
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
            "Windows rejected the synthetic mouse {} event (possibly blocked by UIPI)",
            if down { "down" } else { "up" }
        ))
    }
}

fn pair_matches_target(
    first: Hwnd,
    second: Hwnd,
    orientation: SplitOrientation,
    target: i32,
) -> bool {
    if is_arranged(first) != Some(true) || is_arranged(second) != Some(true) {
        return false;
    }
    let Some(first_rect) = frame_bounds(first) else {
        return false;
    };
    let Some(second_rect) = frame_bounds(second) else {
        return false;
    };

    match orientation {
        SplitOrientation::SideBySide => {
            (first_rect.right - target).abs() <= CUSTOM_TARGET_TOLERANCE
                && (second_rect.left - target).abs() <= CUSTOM_TARGET_TOLERANCE
        }
        SplitOrientation::Stacked => {
            (first_rect.bottom - target).abs() <= CUSTOM_TARGET_TOLERANCE
                && (second_rect.top - target).abs() <= CUSTOM_TARGET_TOLERANCE
        }
    }
}

fn pair_mismatch_description(
    first: Hwnd,
    second: Hwnd,
    orientation: SplitOrientation,
    target: i32,
) -> String {
    let arranged = (is_arranged(first), is_arranged(second));
    let first_rect = frame_bounds(first);
    let second_rect = frame_bounds(second);

    match (first_rect, second_rect) {
        (Some(first_rect), Some(second_rect)) => {
            let (first_edge, second_edge) = match orientation {
                SplitOrientation::SideBySide => (first_rect.right, second_rect.left),
                SplitOrientation::Stacked => (first_rect.bottom, second_rect.top),
            };
            format!(
                "Windows left arranged state={:?}/{:?} with edges {first_edge}/{second_edge}, target {target}",
                arranged.0, arranged.1
            )
        }
        _ => format!(
            "Windows left arranged state={:?}/{:?} and the final frame bounds could not be read",
            arranged.0, arranged.1
        ),
    }
}

fn frame_bounds(hwnd: Hwnd) -> Option<NativeRect> {
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
        return Some(rect);
    }
    (unsafe { GetWindowRect(hwnd, &mut rect) } != 0).then_some(rect)
}

pub(crate) fn restore_resized_pair(
    first_hwnd: usize,
    second_hwnd: usize,
    orientation: SplitOrientation,
    work_area: [i32; 4],
    divider_fraction: f64,
) -> Result<(), String> {
    if !(0.05..=0.95).contains(&divider_fraction) || !divider_fraction.is_finite() {
        return Err(format!(
            "saved custom snap divider fraction {divider_fraction:.4} is outside the safe range"
        ));
    }

    let first = first_hwnd as Hwnd;
    let second = second_hwnd as Hwnd;
    if first.is_null() || second.is_null() || first == second {
        return Err("custom snap pair has invalid window handles".to_owned());
    }

    let width = work_area[2].saturating_sub(work_area[0]);
    let height = work_area[3].saturating_sub(work_area[1]);
    if width <= 0 || height <= 0 {
        return Err("custom snap pair has an invalid monitor work area".to_owned());
    }

    let target = match orientation {
        SplitOrientation::SideBySide => {
            work_area[0] + (divider_fraction * width as f64).round() as i32
        }
        SplitOrientation::Stacked => {
            work_area[1] + (divider_fraction * height as f64).round() as i32
        }
    };

    let mut errors = Vec::new();

    for first_side in [true, false] {
        establish_pair(first, second, orientation)?;

        let primary = if first_side { first } else { second };
        let secondary = if first_side { second } else { first };

        match drag_saved_edge(primary, first_side, orientation, target) {
            Ok(()) => {
                if pair_matches_target(first, second, orientation, target) {
                    return Ok(());
                }

                if is_arranged(primary) == Some(true) && is_arranged(secondary) == Some(true) {
                    if let Err(error) = drag_saved_edge(secondary, !first_side, orientation, target)
                    {
                        errors.push(format!(
                            "{} edge reached the target but the partner edge could not be resized: {error}",
                            if first_side { "first" } else { "second" }
                        ));
                    } else if pair_matches_target(first, second, orientation, target) {
                        return Ok(());
                    }
                }

                errors.push(format!(
                    "{} edge drag completed, but {}",
                    if first_side { "first" } else { "second" },
                    pair_mismatch_description(first, second, orientation, target)
                ));
            }
            Err(error) => errors.push(format!(
                "{} divider-facing edge: {error}",
                if first_side { "first" } else { "second" }
            )),
        }
    }

    establish_pair(first, second, orientation)?;
    if let Some(start) = shared_divider_fallback(first, second, work_area, orientation) {
        let end = match orientation {
            SplitOrientation::SideBySide => (target, start.1),
            SplitOrientation::Stacked => (start.0, target),
        };
        match drag_divider(start, end) {
            Ok(()) if pair_matches_target(first, second, orientation, target) => return Ok(()),
            Ok(()) => errors.push(format!(
                "shared-divider fallback completed, but {}",
                pair_mismatch_description(first, second, orientation, target)
            )),
            Err(error) => errors.push(format!("shared-divider fallback: {error}")),
        }
    } else {
        errors.push("shared-divider fallback could not read the arranged window frames".to_owned());
    }

    Err(format!(
        "could not recreate the native custom snap pair at divider coordinate {target}: {}. Synthetic input can be blocked by UIPI; if either target app is elevated, run Context Capsule from an equally elevated terminal",
        errors.join(" | ")
    ))
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
        assert!(!is_safe_activation_hit(1));
        assert!(!is_safe_activation_hit(8));
        assert!(!is_safe_activation_hit(9));
        assert!(!is_safe_activation_hit(20));
    }

    #[test]
    fn screen_coordinates_preserve_negative_monitor_positions() {
        let packed = screen_point_lparam(-120, -30).expect("packed coordinate") as u32;
        assert_eq!(packed as u16 as i16, -120);
        assert_eq!((packed >> 16) as u16 as i16, -30);
    }

    #[test]
    fn native_input_layout_matches_win32_abi() {
        let expected = if cfg!(target_pointer_width = "64") {
            40
        } else {
            28
        };
        assert_eq!(size_of::<NativeInput>(), expected);
    }
}
