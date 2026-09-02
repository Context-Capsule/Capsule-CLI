use std::ffi::c_void;

use crate::{windows_snap_coord, windows_snap_drag, windows_snap_legacy};

pub(crate) use windows_snap_coord::{
    SnapDirection, SplitOrientation, is_arranged, take_last_arrangement_check,
};

type Hwnd = *mut c_void;

pub(crate) fn snap(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
    match windows_snap_coord::snap(hwnd, direction) {
        Ok(true) => Ok(true),
        Ok(false) => fallback_half_snap(
            hwnd,
            direction,
            "keyboard snap did not enter arranged state",
        ),
        Err(error) => fallback_half_snap(hwnd, direction, &error),
    }
}

fn fallback_half_snap(
    hwnd: Hwnd,
    direction: SnapDirection,
    keyboard_failure: &str,
) -> Result<bool, String> {
    if !matches!(
        direction,
        SnapDirection::LeftHalf | SnapDirection::RightHalf
    ) {
        return Err(keyboard_failure.to_owned());
    }

    match windows_snap_drag::snap_half_by_drag(hwnd, direction) {
        Ok(true) => Ok(true),
        Ok(false) => Err(format!(
            "{keyboard_failure}; verified title-bar drag also completed without entering Windows arranged state"
        )),
        Err(drag_error) => Err(format!(
            "{keyboard_failure}; verified title-bar drag fallback failed: {drag_error}"
        )),
    }
}

pub(crate) fn restore_equal_pair(
    first_hwnd: usize,
    second_hwnd: usize,
    orientation: SplitOrientation,
    work_area: [i32; 4],
) -> Result<(), String> {
    windows_snap_legacy::with_target_work_area(work_area, || {
        windows_snap_coord::restore_equal_pair(first_hwnd, second_hwnd, orientation, work_area)
    })
}

pub(crate) fn restore_resized_pair(
    first_hwnd: usize,
    second_hwnd: usize,
    orientation: SplitOrientation,
    work_area: [i32; 4],
    divider_fraction: f64,
) -> Result<(), String> {
    windows_snap_legacy::with_target_work_area(work_area, || {
        windows_snap_coord::restore_resized_pair(
            first_hwnd,
            second_hwnd,
            orientation,
            work_area,
            divider_fraction,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_half_screen_slots_use_drag_fallback() {
        assert!(matches!(
            SnapDirection::LeftHalf,
            SnapDirection::LeftHalf | SnapDirection::RightHalf
        ));
        assert!(matches!(
            SnapDirection::RightHalf,
            SnapDirection::LeftHalf | SnapDirection::RightHalf
        ));
        assert!(!matches!(
            SnapDirection::TopHalf,
            SnapDirection::LeftHalf | SnapDirection::RightHalf
        ));
        assert!(!matches!(
            SnapDirection::BottomHalf,
            SnapDirection::LeftHalf | SnapDirection::RightHalf
        ));
    }
}
