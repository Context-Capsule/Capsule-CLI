from pathlib import Path

# Keep the existing verified two-attempt primitive for every existing caller,
# but expose a one-attempt variant for the new portrait 50/50 pair only.
p = Path('src/windows_snap_baseline.rs')
s = p.read_text()
old = r'''pub(crate) fn snap(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
    let mut failures = Vec::new();

    for attempt in 1..=SNAP_ATTEMPTS {
        let expected_work = prepare_floating_baseline(hwnd)?;
        let arranged = windows_snap_core::snap(hwnd, direction)?;
        let actual_work = monitor_work_area(hwnd);

        if arranged && actual_work.is_some_and(|actual| same_work_area(actual, expected_work)) {
            return Ok(true);
        }

        let monitor_detail = match actual_work {
            Some(actual) if same_work_area(actual, expected_work) => {
                format!("monitor remained {:?}", actual)
            }
            Some(actual) => format!(
                "window moved to work area {:?} instead of {:?}",
                actual, expected_work
            ),
            None => "window monitor could not be read after the shortcut".to_owned(),
        };
        failures.push(format!(
            "attempt {attempt}: arranged={arranged}, {monitor_detail}"
        ));

        // Contain the failure immediately. This returns the HWND to a verified
        // unarranged position on the intended monitor before any retry or error.
        prepare_floating_baseline(hwnd)?;
    }

    Err(format!(
        "native snap could not produce the requested arranged state on the intended monitor after {SNAP_ATTEMPTS} verified attempt(s): {}",
        failures.join(" | ")
    ))
}
'''
new = r'''pub(crate) fn snap(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
    snap_with_attempts(hwnd, direction, SNAP_ATTEMPTS)
}

pub(crate) fn snap_once(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
    snap_with_attempts(hwnd, direction, 1)
}

fn snap_with_attempts(
    hwnd: Hwnd,
    direction: SnapDirection,
    attempts: usize,
) -> Result<bool, String> {
    debug_assert!(attempts > 0);
    let mut failures = Vec::new();

    for attempt in 1..=attempts {
        let expected_work = prepare_floating_baseline(hwnd)?;
        let arranged = windows_snap_core::snap(hwnd, direction)?;
        let actual_work = monitor_work_area(hwnd);

        if arranged && actual_work.is_some_and(|actual| same_work_area(actual, expected_work)) {
            return Ok(true);
        }

        let monitor_detail = match actual_work {
            Some(actual) if same_work_area(actual, expected_work) => {
                format!("monitor remained {:?}", actual)
            }
            Some(actual) => format!(
                "window moved to work area {:?} instead of {:?}",
                actual, expected_work
            ),
            None => "window monitor could not be read after the shortcut".to_owned(),
        };
        failures.push(format!(
            "attempt {attempt}: arranged={arranged}, {monitor_detail}"
        ));

        // Only establish another baseline when another attempt is actually
        // going to run. A one-shot caller therefore does not visibly replay the
        // same operation after an unchanged failure.
        if attempt < attempts {
            prepare_floating_baseline(hwnd)?;
        }
    }

    Err(format!(
        "native snap could not produce the requested arranged state on the intended monitor after {attempts} verified attempt(s): {}",
        failures.join(" | ")
    ))
}
'''
if 'pub(crate) fn snap_once(' not in s:
    if old not in s:
        raise SystemExit('baseline snap function marker missing')
    s = s.replace(old, new, 1)
    p.write_text(s)

# In the hardened coordinator, leave establish_pair() unchanged for existing
# custom-ratio layouts. The new canonical portrait path alone uses snap_once().
p = Path('src/windows_snap_coord.rs')
s = p.read_text()
if 'fn snap_once(hwnd: Hwnd' not in s:
    marker = r'''fn establish_pair(first: Hwnd, second: Hwnd, orientation: SplitOrientation) -> Result<(), String> {
'''
    addition = r'''fn snap_once(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
    let _attachment = InputQueueAttachment::for_windows(&[hwnd as usize])?;
    activate_target(hwnd)?;
    windows_snap_legacy::snap_once(hwnd, direction)
}

fn establish_pair_once(
    first: Hwnd,
    second: Hwnd,
    orientation: SplitOrientation,
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
    if !snap_once(second, second_direction)? {
        return Err(
            "Windows did not arrange the second window while creating the one-shot stock snap pair"
                .to_owned(),
        );
    }
    thread::sleep(SNAP_SETTLE);

    if is_arranged(first) != Some(true) || is_arranged(second) != Some(true) {
        return Err(
            "one of the windows left arranged state while creating the one-shot stock snap pair"
                .to_owned(),
        );
    }
    Ok(())
}

'''
    if marker not in s:
        raise SystemExit('coordinator establish_pair marker missing')
    s = s.replace(marker, addition + marker, 1)

old_call = '    establish_pair(first, second, orientation)?;\n\n    if equal_pair_matches_work_area('
new_call = '    establish_pair_once(first, second, orientation)?;\n\n    if equal_pair_matches_work_area('
if old_call in s:
    s = s.replace(old_call, new_call, 1)
elif 'establish_pair_once(first, second, orientation)?;' not in s:
    raise SystemExit('restore_equal_pair call marker missing')
p.write_text(s)
