use std::{
    cell::Cell,
    ffi::c_void,
    mem::{size_of, transmute},
    sync::OnceLock,
    thread,
    time::Duration,
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
const VK_LEFT: u16 = 0x25;
const VK_UP: u16 = 0x26;
const VK_RIGHT: u16 = 0x27;
const VK_DOWN: u16 = 0x28;
const VK_MENU: u16 = 0x12;
const VK_LWIN: u16 = 0x5B;
const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;

const FOREGROUND_SETTLE: Duration = Duration::from_millis(45);
const SNAP_SETTLE: Duration = Duration::from_millis(220);
const DIVIDER_HOVER_SETTLE: Duration = Duration::from_millis(90);
const DIVIDER_STEP_SETTLE: Duration = Duration::from_millis(12);
const DIVIDER_RESULT_SETTLE: Duration = Duration::from_millis(280);
const CUSTOM_TARGET_TOLERANCE: i32 = 24;

static IS_WINDOW_ARRANGED: OnceLock<Option<IsWindowArrangedFn>> = OnceLock::new();

thread_local! {
    // Capture currently asks IsWindowArranged immediately before passing the
    // window's normalized geometry into the snap classifier. Keep that result
    // thread-local so the classifier can distinguish a genuinely arranged
    // custom ratio from the legacy geometry-only fallback used when the API is
    // unavailable. The value is consumed by the classifier.
    static LAST_ARRANGEMENT_CHECK: Cell<Option<bool>> = const { Cell::new(None) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapDirection {
    LeftHalf,
    RightHalf,
    TopHalf,
    BottomHalf,
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

#[link(name = "user32")]
unsafe extern "system" {
    fn GetForegroundWindow() -> Hwnd;
    fn SetForegroundWindow(hwnd: Hwnd) -> Bool;
    fn SendInput(count: u32, inputs: *const NativeInput, size: i32) -> u32;
    fn SetCursorPos(x: i32, y: i32) -> Bool;
    fn GetCursorPos(point: *mut Point) -> Bool;
    fn GetWindowRect(hwnd: Hwnd, rect: *mut NativeRect) -> Bool;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(module_name: *const u16) -> Handle;
    fn GetProcAddress(module: Handle, procedure_name: *const u8) -> *mut c_void;
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

/// Returns whether Windows considers this HWND arranged/snapped.
///
/// `IsWindowArranged` exists on modern Windows but deliberately has no import
/// library. Resolve it dynamically so Context Capsule still runs on systems
/// where the export is unavailable.
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

/// Consumes the most recent IsWindowArranged result on this thread.
///
/// This is intentionally narrow: desktop capture invokes `is_arranged` and
/// then the geometry classifier synchronously on the same thread. Consuming
/// the value prevents an old arranged result from leaking into an unrelated
/// later classification.
pub(crate) fn take_last_arrangement_check() -> Option<bool> {
    LAST_ARRANGEMENT_CHECK.with(Cell::take)
}

/// Ask Windows itself to snap the exact target HWND using its documented
/// keyboard gesture. The caller must first stage the window on the desired
/// monitor. No key is injected unless the exact HWND becomes foreground and
/// `IsWindowArranged` is available to verify the result.
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

    if unsafe { GetForegroundWindow() } != hwnd {
        if unsafe { SetForegroundWindow(hwnd) } == 0 {
            return Err("Windows foreground-lock policy refused focus for native snap".to_owned());
        }
        thread::sleep(FOREGROUND_SETTLE);
    }
    if unsafe { GetForegroundWindow() } != hwnd {
        return Err(
            "the intended window did not become foreground; native snap shortcut was not sent"
                .to_owned(),
        );
    }

    let (modifiers, arrow): (&[u16], u16) = match direction {
        SnapDirection::LeftHalf => (&[VK_LWIN], VK_LEFT),
        SnapDirection::RightHalf => (&[VK_LWIN], VK_RIGHT),
        SnapDirection::TopHalf => (&[VK_LWIN, VK_MENU], VK_UP),
        SnapDirection::BottomHalf => (&[VK_LWIN, VK_MENU], VK_DOWN),
    };
    send_chord(modifiers, arrow)?;
    thread::sleep(SNAP_SETTLE);
    Ok(is_arranged(hwnd).unwrap_or(false))
}

/// Recreate a two-window custom Windows Snap split.
///
/// There is no generally available cross-process Win32 setter for arbitrary
/// arranged state. Instead, establish a real native 50/50 pair using Windows'
/// own snap shortcuts, then emulate the user's divider drag to the saved ratio.
/// Windows keeps both windows arranged while resizing adjacent snapped windows.
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

    let mut last_error = String::new();
    for candidate_index in 0..3 {
        establish_pair(first, second, orientation)?;

        let first_rect = frame_bounds(first)
            .ok_or_else(|| "could not read first snapped window frame".to_owned())?;
        let second_rect = frame_bounds(second)
            .ok_or_else(|| "could not read second snapped window frame".to_owned())?;

        let start = divider_start_candidate(
            first_rect,
            second_rect,
            work_area,
            orientation,
            candidate_index,
        );
        let end = match orientation {
            SplitOrientation::SideBySide => (target, start.1),
            SplitOrientation::Stacked => (start.0, target),
        };

        if let Err(error) = drag_divider(start, end) {
            last_error = error;
            continue;
        }

        if pair_matches_target(first, second, orientation, target) {
            return Ok(());
        }

        last_error = format!(
            "Windows accepted the divider drag but the pair did not remain arranged at the requested divider coordinate {target}"
        );
    }

    Err(if last_error.is_empty() {
        "could not recreate the native custom snap pair".to_owned()
    } else {
        format!(
            "could not recreate the native custom snap pair: {last_error}. Synthetic input can be blocked by UIPI; if either target app is elevated, run Context Capsule from an equally elevated terminal"
        )
    })
}

fn establish_pair(
    first: Hwnd,
    second: Hwnd,
    orientation: SplitOrientation,
) -> Result<(), String> {
    let (first_direction, second_direction) = match orientation {
        SplitOrientation::SideBySide => (SnapDirection::LeftHalf, SnapDirection::RightHalf),
        SplitOrientation::Stacked => (SnapDirection::TopHalf, SnapDirection::BottomHalf),
    };

    if !snap(first, first_direction)? {
        return Err("Windows did not arrange the first window while creating the custom snap pair"
            .to_owned());
    }
    if !snap(second, second_direction)? {
        return Err("Windows did not arrange the second window while creating the custom snap pair"
            .to_owned());
    }
    thread::sleep(SNAP_SETTLE);

    if is_arranged(first) != Some(true) || is_arranged(second) != Some(true) {
        return Err("one of the windows left arranged state while creating the custom snap pair"
            .to_owned());
    }
    Ok(())
}

fn divider_start_candidate(
    first: NativeRect,
    second: NativeRect,
    work_area: [i32; 4],
    orientation: SplitOrientation,
    candidate_index: usize,
) -> (i32, i32) {
    match orientation {
        SplitOrientation::SideBySide => {
            let center = (first.right as i64 + second.left as i64) / 2;
            let x = match candidate_index {
                1 => first.right.saturating_sub(1) as i64,
                2 => second.left.saturating_add(1) as i64,
                _ => center,
            } as i32;
            let y = work_area[1] + work_area[3].saturating_sub(work_area[1]) / 2;
            (x, y)
        }
        SplitOrientation::Stacked => {
            let center = (first.bottom as i64 + second.top as i64) / 2;
            let y = match candidate_index {
                1 => first.bottom.saturating_sub(1) as i64,
                2 => second.top.saturating_add(1) as i64,
                _ => center,
            } as i32;
            let x = work_area[0] + work_area[2].saturating_sub(work_area[0]) / 2;
            (x, y)
        }
    }
}

fn drag_divider(start: (i32, i32), end: (i32, i32)) -> Result<(), String> {
    let mut original = Point::default();
    let have_original = unsafe { GetCursorPos(&mut original) } != 0;

    if unsafe { SetCursorPos(start.0, start.1) } == 0 {
        return Err("SetCursorPos failed while targeting the Windows snap divider".to_owned());
    }
    thread::sleep(DIVIDER_HOVER_SETTLE);

    send_mouse_button(true)?;
    let steps = 18_i32;
    for step in 1..=steps {
        let x = start.0 as i64
            + ((end.0 as i64 - start.0 as i64) * step as i64) / steps as i64;
        let y = start.1 as i64
            + ((end.1 as i64 - start.1 as i64) * step as i64) / steps as i64;
        if unsafe { SetCursorPos(x as i32, y as i32) } == 0 {
            let _ = send_mouse_button(false);
            if have_original {
                unsafe {
                    SetCursorPos(original.x, original.y);
                }
            }
            return Err("SetCursorPos failed during the Windows snap divider drag".to_owned());
        }
        thread::sleep(DIVIDER_STEP_SETTLE);
    }

    let release_result = send_mouse_button(false);
    thread::sleep(DIVIDER_RESULT_SETTLE);
    if have_original {
        unsafe {
            SetCursorPos(original.x, original.y);
        }
    }
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
    let sent = unsafe {
        SendInput(
            expected,
            inputs.as_ptr(),
            size_of::<NativeInput>() as i32,
        )
    };
    if sent == expected {
        return Ok(());
    }

    // If Windows accepted only part of the batch, make a best-effort key-up
    // pass so a synthetic Windows/Alt key can never remain logically held.
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
        let expected = if cfg!(target_pointer_width = "64") { 40 } else { 28 };
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
    fn arrangement_hint_is_consumed_once() {
        LAST_ARRANGEMENT_CHECK.with(|last| last.set(Some(true)));
        assert_eq!(take_last_arrangement_check(), Some(true));
        assert_eq!(take_last_arrangement_check(), None);
    }

    #[test]
    fn divider_candidate_uses_shared_edge_center_first() {
        let first = NativeRect {
            left: 0,
            top: 0,
            right: 955,
            bottom: 1040,
        };
        let second = NativeRect {
            left: 965,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        assert_eq!(
            divider_start_candidate(
                first,
                second,
                [0, 0, 1920, 1040],
                SplitOrientation::SideBySide,
                0,
            ),
            (960, 520)
        );
    }
}