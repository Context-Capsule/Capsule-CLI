use std::ffi::c_void;

use crate::{windows_snap_coord, windows_snap_legacy};

pub(crate) use windows_snap_coord::{
    SnapDirection, SplitOrientation, is_arranged, take_last_arrangement_check,
};

type Hwnd = *mut c_void;

pub(crate) fn snap(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
    windows_snap_coord::snap(hwnd, direction)
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
