use std::{
    ffi::c_void,
    mem::{size_of, transmute},
    sync::OnceLock,
    thread,
    time::Duration,
};

type Hwnd = *mut c_void;
type Handle = *mut c_void;
type Bool = i32;
type IsWindowArrangedFn = unsafe extern "system" fn(Hwnd) -> Bool;

const INPUT_KEYBOARD: u32 = 1;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const VK_LEFT: u16 = 0x25;
const VK_UP: u16 = 0x26;
const VK_RIGHT: u16 = 0x27;
const VK_DOWN: u16 = 0x28;
const VK_MENU: u16 = 0x12;
const VK_LWIN: u16 = 0x5B;

const FOREGROUND_SETTLE: Duration = Duration::from_millis(45);
const SNAP_SETTLE: Duration = Duration::from_millis(180);

static IS_WINDOW_ARRANGED: OnceLock<Option<IsWindowArrangedFn>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapDirection {
    LeftHalf,
    RightHalf,
    TopHalf,
    BottomHalf,
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
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(module_name: *const u16) -> Handle;
    fn GetProcAddress(module: Handle, procedure_name: *const u8) -> *mut c_void;
}

/// Returns whether Windows considers this HWND arranged/snapped.
///
/// `IsWindowArranged` exists on modern Windows but deliberately has no import
/// library. Resolve it dynamically so Context Capsule still runs on systems
/// where the export is unavailable.
pub(crate) fn is_arranged(hwnd: Hwnd) -> Option<bool> {
    if hwnd.is_null() {
        return None;
    }
    match *IS_WINDOW_ARRANGED.get_or_init(resolve_is_window_arranged) {
        Some(function) => Some(unsafe { function(hwnd) != 0 }),
        None => None,
    }
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
}
