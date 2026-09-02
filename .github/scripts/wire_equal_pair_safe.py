from pathlib import Path

# Expose a one-shot 50/50 pair through the hardened coordination layer.
p = Path('src/windows_snap_coord.rs')
s = p.read_text()
if 'pub(crate) fn restore_equal_pair(' not in s:
    marker = '''fn resize_hit_code(first_side: bool, orientation: SplitOrientation) -> i32 {'''
    block = r'''pub(crate) fn restore_equal_pair(
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
    establish_pair(first, second, orientation)?;

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

'''
    if marker not in s:
        raise SystemExit('coord insertion marker missing')
    s = s.replace(marker, block + marker, 1)
    p.write_text(s)

p = Path('src/windows_snap_safe.rs')
s = p.read_text()
if 'pub(crate) fn restore_equal_pair(' not in s:
    marker = '''pub(crate) fn restore_resized_pair(
'''
    block = r'''pub(crate) fn restore_equal_pair(
    first_hwnd: usize,
    second_hwnd: usize,
    orientation: SplitOrientation,
    work_area: [i32; 4],
) -> Result<(), String> {
    windows_snap_legacy::with_target_work_area(work_area, || {
        windows_snap_coord::restore_equal_pair(
            first_hwnd,
            second_hwnd,
            orientation,
            work_area,
        )
    })
}

'''
    if marker not in s:
        raise SystemExit('safe wrapper insertion marker missing')
    s = s.replace(marker, block + marker, 1)
    p.write_text(s)
