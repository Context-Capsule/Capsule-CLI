use std::{
    cell::Cell,
    ffi::c_void,
    mem::{size_of, transmute},
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

type Hwnd = *mut c_void;
type Handle = *mut c_void;
type Bool = i32;
type Hresult = i32;
type IsWindowArrangedFn = unsafe extern "system" fn(Hwnd) -> Bool;

const INPUT_MOUSE: u32 = 0;
const INPUT_KEYBOARD: u32 = 1;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
const VK_ESCAPE: u16 = 0x1B;
const VK_LEFT: u16 = 0x25;
const VK_UP: u16 = 0x26;
const VK_RIGHT: u16 = 0x27;
const VK_DOWN: u16 = 0x28;
const VK_LWIN: u16 = 0x5B;
const VK_Z: u16 = 0x5A;
const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;

const WM_NCHITTEST: u32 = 0x0084;
const SMTO_ABORTIFHUNG: u32 = 0x0002;
const HTLEFT: i32 = 10;
const HTRIGHT: i32 = 11;
const HTTOP: i32 = 12;
const HTBOTTOM: i32 = 15;

const FOREGROUND_SETTLE: Duration = Duration::from_millis(45);
const FOREGROUND_ACQUIRE_TIMEOUT: Duration = Duration::from_millis(1_400);
const FOREGROUND_POLL_INTERVAL: Duration = Duration::from_millis(20);
const SNAP_SETTLE: Duration = Duration::from_millis(220);
const SNAP_PATH_STEP_SETTLE: Duration = Duration::from_millis(180);
const SNAP_LAYOUT_OPEN_SETTLE: Duration = Duration::from_millis(300);
const SNAP_LAYOUT_SELECT_SETTLE: Duration = Duration::from_millis(240);
const SNAP_LAYOUT_RESULT_TIMEOUT: Duration = Duration::from_millis(1_500);
const SNAP_LAYOUT_DISMISS_SETTLE: Duration = Duration::from_millis(100);
const DIVIDER_HOVER_SETTLE: Duration = Duration::from_millis(90);
const DIVIDER_STEP_SETTLE: Duration = Duration::from_millis(12);
const DIVIDER_RESULT_SETTLE: Duration = Duration::from_millis(280);
const CUSTOM_TARGET_TOLERANCE: i32 = 24;
const HIT_TEST_TIMEOUT_MS: u32 = 120;

const EDGE_SCAN_OFFSETS: [i32; 21] = [
    0, -1, 1, -2, 2, -3, 3, -4, 4, -5, 5, -6, 6, -8, 8, -10, 10, -12, 12, -16, 16,
];

static IS_WINDOW_ARRANGED: OnceLock<Option<IsWindowArrangedFn>> = OnceLock::new();

thread_local! {
    static LAST_ARRANGEMENT_CHECK: Cell<Option<bool>> = const { Cell::new(None) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapDirection {
    LeftHalf,
    RightHalf,
    TopHalf,
    BottomHalf,
    TopLeftQuarter,
    TopRightQuarter,
    BottomLeftQuarter,
    BottomRightQuarter,
    LeftThird,
    CenterThird,
    RightThird,
    LeftTwoThirds,
    RightTwoThirds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SplitOrientation {
    SideBySide,
    Stacked,
}

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

struct ForegroundRestoreGuard {
    hwnd: Hwnd,
}

impl Drop for ForegroundRestoreGuard {
    fn drop(&mut self) {
        if !self.hwnd.is_null() && unsafe { GetForegroundWindow() } != self.hwnd {
            unsafe {
                SetForegroundWindow(self.hwnd);
            }
        }
    }
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
    fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
    fn GetKeyboardLayout(thread_id: u32) -> Handle;
    fn VkKeyScanExW(character: u16, keyboard_layout: Handle) -> i16;
    fn AttachThreadInput(id_attach: u32, id_attach_to: u32, attach: Bool) -> Bool;
    fn SendInput(count: u32, inputs: *const NativeInput, size: i32) -> u32;
    fn SetCursorPos(x: i32, y: i32) -> Bool;
    fn GetCursorPos(point: *mut Point) -> Bool;
    fn GetWindowRect(hwnd: Hwnd, rect: *mut NativeRect) -> Bool;
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
    fn GetModuleHandleW(module_name: *const u16) -> Handle;
    fn GetProcAddress(module: Handle, procedure_name: *const u8) -> *mut c_void;
    fn GetCurrentThreadId() -> u32;
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

pub(crate) fn is_arranged(hwnd: Hwnd) -> Option<bool> {
    let result = if hwnd.is_null() {
        None
    } else {
        match *IS_WINDOW_ARRANGED.get_or_init(resolve_is_window_arranged) {
            Some(function) => Some(unsafe { function(hwnd) != 0 }),
            None => None,
        }
    };
    LAST_ARRANGEMENT_CHECK.with(|last| last.set(result));
    result
}

pub(crate) fn take_last_arrangement_check() -> Option<bool> {
    LAST_ARRANGEMENT_CHECK.with(Cell::take)
}

pub(crate) fn snap(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
    if hwnd.is_null() {
        return Err("window handle is unavailable".to_owned());
    }
    if is_arranged(hwnd).is_none() {
        return Err(
            "Windows does not expose IsWindowArranged; refusing to inject a snap shortcut without post-action verification"
                .to_owned(),
        );
    }

    if !focus_window_without_geometry_change(hwnd) {
        return Err(
            "Windows foreground-lock policy prevented focusing the intended window for native snap"
                .to_owned(),
        );
    }

    match direction {
        SnapDirection::LeftHalf => send_chord(&[VK_LWIN], VK_LEFT)?,
        SnapDirection::RightHalf => send_chord(&[VK_LWIN], VK_RIGHT)?,
        SnapDirection::TopHalf => send_chord(&[VK_LWIN, VK_MENU], VK_UP)?,
        SnapDirection::BottomHalf => send_chord(&[VK_LWIN, VK_MENU], VK_DOWN)?,
        SnapDirection::TopLeftQuarter => send_win_arrow_path(&[VK_LEFT, VK_UP])?,
        SnapDirection::TopRightQuarter => send_win_arrow_path(&[VK_RIGHT, VK_UP])?,
        SnapDirection::BottomLeftQuarter => send_win_arrow_path(&[VK_LEFT, VK_DOWN])?,
        SnapDirection::BottomRightQuarter => send_win_arrow_path(&[VK_RIGHT, VK_DOWN])?,
        direction => {
            let (layout, zone) = snap_layout_choice(direction).ok_or_else(|| {
                format!("no native Snap Layout mapping is available for {direction:?}")
            })?;
            return snap_layout_zone(hwnd, layout, zone);
        }
    }

    thread::sleep(SNAP_SETTLE);
    Ok(is_arranged(hwnd).unwrap_or(false))
}

fn snap_layout_choice(direction: SnapDirection) -> Option<(u8, u8)> {
    Some(match direction {
        // Windows 11 numbers the standard landscape templates in the Win+Z
        // flyout as: 1=halves, 2=2/3+1/3, 3=1/3+2/3, 4=three thirds,
        // 5=half+two quarters, 6=four quarters. A one-third slot is chosen from
        // an asymmetric two-zone template when possible so a paired 2/3 window
        // can share the same native template; center-third necessarily uses 4.
        SnapDirection::LeftThird => (3, 1),
        SnapDirection::CenterThird => (4, 2),
        SnapDirection::RightThird => (2, 2),
        SnapDirection::LeftTwoThirds => (2, 1),
        SnapDirection::RightTwoThirds => (3, 2),
        _ => return None,
    })
}

fn snap_layout_zone(hwnd: Hwnd, layout: u8, zone: u8) -> Result<bool, String> {
    if !(1..=9).contains(&layout) || !(1..=9).contains(&zone) {
        return Err(format!("invalid Snap Layout access key {layout}:{zone}"));
    }
    if unsafe { GetForegroundWindow() } != hwnd {
        return Err(
            "the native Snap Layout target lost foreground focus before Win+Z; no layout keys were sent"
                .to_owned(),
        );
    }

    // No helper/probe HWND is created. Win+Z is opened only for the real target
    // window, and every access-key stage re-checks that the intended HWND still
    // owns foreground focus before continuing.
    send_chord(&[VK_LWIN], VK_Z)?;
    thread::sleep(SNAP_LAYOUT_OPEN_SETTLE);

    let result = (|| {
        if unsafe { GetForegroundWindow() } != hwnd {
            return Err(
                "the Snap Layout flyout did not remain associated with the intended target window"
                    .to_owned(),
            );
        }
        send_access_digit(hwnd, layout)?;
        thread::sleep(SNAP_LAYOUT_SELECT_SETTLE);
        if unsafe { GetForegroundWindow() } != hwnd {
            return Err(
                "the intended target lost foreground focus while selecting a Snap Layout"
                    .to_owned(),
            );
        }
        send_access_digit(hwnd, zone)?;

        let deadline = Instant::now() + SNAP_LAYOUT_RESULT_TIMEOUT;
        while Instant::now() < deadline {
            if is_arranged(hwnd) == Some(true) {
                return Ok(true);
            }
            thread::sleep(FOREGROUND_POLL_INTERVAL);
        }
        Ok(false)
    })();

    // Native zone selection normally opens Snap Assist. Escape dismisses that
    // transient shell picker without undoing the arranged target window. It also
    // guarantees a failed restore does not leave a Win+Z overlay behind.
    let _ = send_chord(&[], VK_ESCAPE);
    thread::sleep(SNAP_LAYOUT_DISMISS_SETTLE);
    result.map(|arranged| arranged && is_arranged(hwnd) == Some(true))
}

fn send_access_digit(hwnd: Hwnd, digit: u8) -> Result<(), String> {
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };
    if thread_id == 0 {
        return Err(
            "could not resolve the target keyboard layout for Snap Layout access keys".to_owned(),
        );
    }
    let keyboard_layout = unsafe { GetKeyboardLayout(thread_id) };
    let mapping = unsafe { VkKeyScanExW(u16::from(b'0' + digit), keyboard_layout) };
    if mapping == -1 {
        return Err(format!(
            "the target keyboard layout cannot generate Snap Layout digit '{digit}'"
        ));
    }

    let virtual_key = (mapping as u16) & 0x00ff;
    let shift_state = ((mapping as u16) >> 8) & 0x00ff;
    let mut modifiers = Vec::with_capacity(3);
    if shift_state & 0x01 != 0 {
        modifiers.push(VK_SHIFT);
    }
    if shift_state & 0x02 != 0 {
        modifiers.push(VK_CONTROL);
    }
    if shift_state & 0x04 != 0 {
        modifiers.push(VK_MENU);
    }
    send_chord(&modifiers, virtual_key)
}

fn send_win_arrow_path(arrows: &[u16]) -> Result<(), String> {
    for (index, arrow) in arrows.iter().enumerate() {
        send_chord(&[VK_LWIN], *arrow)?;
        if index + 1 < arrows.len() {
            thread::sleep(SNAP_PATH_STEP_SETTLE);
        }
    }
    Ok(())
}

fn focus_window_without_geometry_change(hwnd: Hwnd) -> bool {
    if hwnd.is_null() {
        return false;
    }
    if unsafe { GetForegroundWindow() } == hwnd {
        return true;
    }

    unsafe {
        SetForegroundWindow(hwnd);
    }
    if wait_until_foreground(hwnd, Duration::from_millis(250)) {
        return true;
    }

    // Windows can reject SetForegroundWindow even for a visible interactive
    // window. Temporarily join the caller's input queue with the foreground and
    // target window threads, retry focus, then detach immediately. This changes
    // only keyboard focus/z-order; it does not restore, resize or reposition the
    // window, so a saved Snap layout cannot be destroyed by focus acquisition.
    let current_thread = unsafe { GetCurrentThreadId() };
    let foreground = unsafe { GetForegroundWindow() };
    let foreground_thread = if foreground.is_null() {
        0
    } else {
        unsafe { GetWindowThreadProcessId(foreground, std::ptr::null_mut()) }
    };
    let target_thread = unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };

    let attach_foreground = foreground_thread != 0 && foreground_thread != current_thread;
    let attach_target =
        target_thread != 0 && target_thread != current_thread && target_thread != foreground_thread;

    if attach_foreground {
        unsafe {
            AttachThreadInput(current_thread, foreground_thread, 1);
        }
    }
    if attach_target {
        unsafe {
            AttachThreadInput(current_thread, target_thread, 1);
        }
    }

    unsafe {
        SetForegroundWindow(hwnd);
    }

    if attach_target {
        unsafe {
            AttachThreadInput(current_thread, target_thread, 0);
        }
    }
    if attach_foreground {
        unsafe {
            AttachThreadInput(current_thread, foreground_thread, 0);
        }
    }

    wait_until_foreground(hwnd, FOREGROUND_ACQUIRE_TIMEOUT)
}

fn wait_until_foreground(hwnd: Hwnd, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if unsafe { GetForegroundWindow() } == hwnd {
            return true;
        }
        thread::sleep(FOREGROUND_POLL_INTERVAL);
    }
    false
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

    let _foreground_restore = ForegroundRestoreGuard {
        hwnd: unsafe { GetForegroundWindow() },
    };

    // A canonical 50/50 pair needs no divider drag. Each member gets exactly
    // one native Snap attempt, followed by verification. If Windows makes no
    // progress, return immediately rather than replaying identical input.
    establish_pair(first, second, orientation)?;
    thread::sleep(SNAP_SETTLE);

    if equal_pair_matches_work_area(first, second, orientation, work_area, 3) {
        Ok(())
    } else {
        Err(format!(
            "Windows created the stock snap pair, but it did not settle into the expected 50/50 work-area halves: {}",
            pair_mismatch_description(
                first,
                second,
                orientation,
                match orientation {
                    SplitOrientation::SideBySide => work_area[0] + width / 2,
                    SplitOrientation::Stacked => work_area[1] + height / 2,
                },
            )
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

    let close = |left: i32, right: i32| (left - right).abs() <= tolerance;
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

    let _foreground_restore = ForegroundRestoreGuard {
        hwnd: unsafe { GetForegroundWindow() },
    };

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

fn drag_resize_handle(
    hwnd: Hwnd,
    expected_hit: i32,
    start: (i32, i32),
    end: (i32, i32),
) -> Result<(), String> {
    if !focus_window_without_geometry_change(hwnd) {
        return Err("Windows foreground-lock policy refused focus for snap resize drag".to_owned());
    }

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

    if let Err(error) = send_mouse_button(true) {
        return Err(error);
    }

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

fn resolve_is_window_arranged() -> Option<IsWindowArrangedFn> {
    let module_name = "user32.dll"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let module = unsafe { GetModuleHandleW(module_name.as_ptr()) };
    if module.is_null() {
        return None;
    }
    let address = unsafe { GetProcAddress(module, b"IsWindowArranged\0".as_ptr()) };
    if address.is_null() {
        return None;
    }
    Some(unsafe { transmute::<*mut c_void, IsWindowArrangedFn>(address) })
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

fn keyboard_input(virtual_key: u16, key_up: bool) -> NativeInput {
    NativeInput {
        kind: INPUT_KEYBOARD,
        payload: InputPayload {
            keyboard: KeyboardInput {
                virtual_key,
                scan_code: 0,
                flags: if key_up { KEYEVENTF_KEYUP } else { 0 },
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
            "Windows rejected the synthetic mouse {} event (possibly blocked by UIPI)",
            if down { "down" } else { "up" }
        ))
    }
}

fn send_chord(modifiers: &[u16], key: u16) -> Result<(), String> {
    let mut inputs = Vec::with_capacity(modifiers.len() * 2 + 2);
    for modifier in modifiers {
        inputs.push(keyboard_input(*modifier, false));
    }
    inputs.push(keyboard_input(key, false));
    inputs.push(keyboard_input(key, true));
    for modifier in modifiers.iter().rev() {
        inputs.push(keyboard_input(*modifier, true));
    }

    let expected = inputs.len() as u32;
    let sent = unsafe { SendInput(expected, inputs.as_ptr(), size_of::<NativeInput>() as i32) };
    if sent == expected {
        return Ok(());
    }

    let mut releases = Vec::with_capacity(modifiers.len() + 1);
    releases.push(keyboard_input(key, true));
    for modifier in modifiers.iter().rev() {
        releases.push(keyboard_input(*modifier, true));
    }
    unsafe {
        SendInput(
            releases.len() as u32,
            releases.as_ptr(),
            size_of::<NativeInput>() as i32,
        );
    }

    Err(format!(
        "Windows accepted {sent}/{expected} synthetic snap key events (possibly blocked by UIPI or another input policy)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_layout_matches_win32_abi() {
        let expected = if cfg!(target_pointer_width = "64") {
            40
        } else {
            28
        };
        assert_eq!(size_of::<NativeInput>(), expected);
    }

    #[test]
    fn keyboard_input_is_initialized_as_keyboard() {
        let input = keyboard_input(VK_LEFT, false);
        assert_eq!(input.kind, INPUT_KEYBOARD);
        let keyboard = unsafe { input.payload.keyboard };
        assert_eq!(keyboard.virtual_key, VK_LEFT);
        assert_eq!(keyboard.flags, 0);
    }

    #[test]
    fn mouse_input_is_initialized_as_mouse() {
        let input = mouse_input(MOUSEEVENTF_LEFTDOWN);
        assert_eq!(input.kind, INPUT_MOUSE);
        let mouse = unsafe { input.payload.mouse };
        assert_eq!(mouse.flags, MOUSEEVENTF_LEFTDOWN);
    }

    #[test]
    fn stock_snap_layout_choices_cover_thirds_and_asymmetric_slots() {
        assert_eq!(snap_layout_choice(SnapDirection::LeftThird), Some((3, 1)));
        assert_eq!(snap_layout_choice(SnapDirection::CenterThird), Some((4, 2)));
        assert_eq!(snap_layout_choice(SnapDirection::RightThird), Some((2, 2)));
        assert_eq!(
            snap_layout_choice(SnapDirection::LeftTwoThirds),
            Some((2, 1))
        );
        assert_eq!(
            snap_layout_choice(SnapDirection::RightTwoThirds),
            Some((3, 2))
        );
        assert_eq!(snap_layout_choice(SnapDirection::TopLeftQuarter), None);
    }

    #[test]
    fn arrangement_hint_is_consumed_once() {
        LAST_ARRANGEMENT_CHECK.with(|last| last.set(Some(true)));
        assert_eq!(take_last_arrangement_check(), Some(true));
        assert_eq!(take_last_arrangement_check(), None);
    }

    #[test]
    fn resize_hit_codes_follow_divider_facing_edges() {
        assert_eq!(resize_hit_code(true, SplitOrientation::SideBySide), HTRIGHT);
        assert_eq!(resize_hit_code(false, SplitOrientation::SideBySide), HTLEFT);
        assert_eq!(resize_hit_code(true, SplitOrientation::Stacked), HTBOTTOM);
        assert_eq!(resize_hit_code(false, SplitOrientation::Stacked), HTTOP);
    }

    #[test]
    fn screen_point_lparam_preserves_negative_monitor_coordinates() {
        let packed = screen_point_lparam(-120, -30).expect("packed coordinate") as u32;
        assert_eq!(packed as u16 as i16, -120);
        assert_eq!((packed >> 16) as u16 as i16, -30);
    }
}
