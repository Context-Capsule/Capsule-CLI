#![cfg(windows)]

use crate::zen_shortcuts_core;
use std::{
    ffi::c_void,
    thread,
    time::Duration,
};

type Hwnd = *mut c_void;
type Bool = i32;

const ACTIVATION_RETRIES: usize = 4;
const ACTIVATION_SETTLE: Duration = Duration::from_millis(35);

#[link(name = "user32")]
unsafe extern "system" {
    fn GetForegroundWindow() -> Hwnd;
    fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
    fn AttachThreadInput(id_attach: u32, id_attach_to: u32, attach: Bool) -> Bool;
    fn BringWindowToTop(hwnd: Hwnd) -> Bool;
    fn SetForegroundWindow(hwnd: Hwnd) -> Bool;
    fn SetActiveWindow(hwnd: Hwnd) -> Hwnd;
    fn SetFocus(hwnd: Hwnd) -> Hwnd;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn GetLastError() -> u32;
}

struct InputQueueAttachment {
    current_thread: u32,
    target_thread: u32,
    attached: bool,
}

impl InputQueueAttachment {
    fn for_window(hwnd: Hwnd) -> Result<Self, String> {
        let current_thread = unsafe { GetCurrentThreadId() };
        let target_thread = unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };
        if target_thread == 0 {
            return Err("could not resolve Zen's foreground GUI thread before split invocation".to_owned());
        }
        if target_thread == current_thread {
            return Ok(Self {
                current_thread,
                target_thread,
                attached: false,
            });
        }

        let attached = unsafe { AttachThreadInput(current_thread, target_thread, 1) } != 0;
        if !attached {
            let error = unsafe { GetLastError() };
            return Err(format!(
                "could not attach Context Capsule's input queue to the foreground Zen thread (Win32 error {error})"
            ));
        }
        Ok(Self {
            current_thread,
            target_thread,
            attached: true,
        })
    }
}

impl Drop for InputQueueAttachment {
    fn drop(&mut self) {
        if self.attached {
            unsafe {
                AttachThreadInput(self.current_thread, self.target_thread, 0);
            }
        }
    }
}

fn establish_foreground_keyboard_target() -> Result<(), String> {
    let target = unsafe { GetForegroundWindow() };
    if target.is_null() {
        return Err("Zen split invocation has no foreground window to activate".to_owned());
    }

    let _attachment = InputQueueAttachment::for_window(target)?;
    let mut last_error = 0_u32;
    for _ in 0..ACTIVATION_RETRIES {
        unsafe {
            BringWindowToTop(target);
            SetForegroundWindow(target);
            SetActiveWindow(target);
            SetFocus(target);
        }
        thread::sleep(ACTIVATION_SETTLE);
        if unsafe { GetForegroundWindow() } == target {
            return Ok(());
        }
        last_error = unsafe { GetLastError() };
    }

    Err(format!(
        "could not establish the exact foreground Zen window as the keyboard target before split invocation (Win32 error {last_error})"
    ))
}

pub(crate) fn invoke_split_shortcut(orientation: &str) -> Result<(), String> {
    // Browser-side code has already focused the exact restored Zen window and
    // selected the exact saved split members. Strengthen that focus at the
    // native GUI-thread level, then delegate only the key-resolution/injection
    // work to the existing implementation. This avoids changing the proven
    // profile shortcut parser or any Windows Snap code.
    establish_foreground_keyboard_target()?;
    zen_shortcuts_core::invoke_split_shortcut(orientation)
}
