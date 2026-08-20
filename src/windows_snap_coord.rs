use std::{collections::HashSet, ffi::c_void};

use crate::windows_snap_legacy;

pub(crate) use windows_snap_legacy::{
    SnapDirection, SplitOrientation, is_arranged, take_last_arrangement_check,
};

type Hwnd = *mut c_void;
type Bool = i32;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetForegroundWindow() -> Hwnd;
    fn SetForegroundWindow(hwnd: Hwnd) -> Bool;
    fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
    fn AttachThreadInput(id_attach: u32, id_attach_to: u32, attach: Bool) -> Bool;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
}

/// Temporarily joins the CLI restore thread to the input queues that own the
/// foreground window and the target windows. Windows deliberately allows
/// SetForegroundWindow to be refused across unrelated input queues; attaching
/// the queues for this short, synchronous native-Snap operation gives the
/// existing snap code a legitimate activation path without weakening its
/// GetForegroundWindow verification or sending keys to an unverified HWND.
struct InputQueueAttachment {
    current_thread: u32,
    attached_threads: Vec<u32>,
}

impl InputQueueAttachment {
    fn for_windows(targets: &[usize]) -> Self {
        let current_thread = unsafe { GetCurrentThreadId() };
        let mut candidates = Vec::new();

        let foreground = unsafe { GetForegroundWindow() };
        if !foreground.is_null() {
            let thread = unsafe { GetWindowThreadProcessId(foreground, std::ptr::null_mut()) };
            if thread != 0 {
                candidates.push(thread);
            }
        }

        for target in targets {
            let hwnd = *target as Hwnd;
            if hwnd.is_null() {
                continue;
            }
            let thread = unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };
            if thread != 0 {
                candidates.push(thread);
            }
        }

        let mut seen = HashSet::new();
        let mut attached_threads = Vec::new();
        for thread in candidates {
            if thread == current_thread || !seen.insert(thread) {
                continue;
            }
            if unsafe { AttachThreadInput(current_thread, thread, 1) } != 0 {
                attached_threads.push(thread);
            }
        }

        Self {
            current_thread,
            attached_threads,
        }
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

pub(crate) fn snap(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
    let _attachment = InputQueueAttachment::for_windows(&[hwnd as usize]);

    // Prime activation while the queues are joined. The legacy implementation
    // immediately verifies the actual foreground HWND before it sends Win+Arrow,
    // so a rejected activation still fails closed rather than targeting the
    // wrong window.
    if !hwnd.is_null() && unsafe { GetForegroundWindow() } != hwnd {
        unsafe {
            SetForegroundWindow(hwnd);
        }
    }

    windows_snap_legacy::snap(hwnd, direction)
}

pub(crate) fn restore_resized_pair(
    first_hwnd: usize,
    second_hwnd: usize,
    orientation: SplitOrientation,
    work_area: [i32; 4],
    divider_fraction: f64,
) -> Result<(), String> {
    // Keep the three relevant input queues joined for the *entire* pair
    // reconstruction. The legacy routine alternates foreground between the two
    // target HWNDs several times while creating the stock pair and dragging the
    // divider, so attaching only around the first activation is insufficient.
    let _attachment = InputQueueAttachment::for_windows(&[first_hwnd, second_hwnd]);

    let first = first_hwnd as Hwnd;
    if !first.is_null() && unsafe { GetForegroundWindow() } != first {
        unsafe {
            SetForegroundWindow(first);
        }
    }

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
}
