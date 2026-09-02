from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    p = Path(path)
    s = p.read_text()
    if new in s:
        return
    if old not in s:
        raise SystemExit(f"{label}: marker missing in {path}")
    p.write_text(s.replace(old, new, 1))


# 1) Keep the existing two-attempt native primitive for existing callers, but
# expose a truly one-attempt variant for the portrait 50/50 pair path.
p = Path("src/windows_snap_baseline.rs")
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

        // Only establish another baseline if another attempt will actually run.
        // The portrait-pair caller uses one attempt, so an unchanged failure is
        // never visibly replayed.
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
if "pub(crate) fn snap_once(" not in s:
    if old not in s:
        raise SystemExit("baseline one-shot marker missing")
    p.write_text(s.replace(old, new, 1))


# 2) The canonical equal-pair coordinator uses the one-shot primitive and checks
# the FIRST window's exact 50/50 zone before it ever touches the second window.
p = Path("src/windows_snap_coord.rs")
s = p.read_text()
if "fn snap_once(hwnd: Hwnd" not in s:
    marker = r'''fn establish_pair(first: Hwnd, second: Hwnd, orientation: SplitOrientation) -> Result<(), String> {
'''
    addition = r'''fn snap_once(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
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
            .map(|rect| format!("[{}, {}, {}, {}]", rect.left, rect.top, rect.right, rect.bottom))
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

'''
    if marker not in s:
        raise SystemExit("coordinator establish_pair marker missing")
    s = s.replace(marker, addition + marker, 1)

old_call = "    establish_pair(first, second, orientation)?;\n\n    if equal_pair_matches_work_area("
new_call = "    establish_pair_once(first, second, orientation, work_area)?;\n\n    if equal_pair_matches_work_area("
if old_call in s:
    s = s.replace(old_call, new_call, 1)
elif "establish_pair_once(first, second, orientation, work_area)?;" not in s:
    raise SystemExit("restore_equal_pair one-shot call marker missing")
p.write_text(s)


# 3) Expose the existing DWM-aware exact placement primitive as a one-pass
# floating fallback. It unconditionally clears Snap first, applies one adjusted
# SetWindowPos, observes once after settling, and never loops.
p = Path("src/restore/windows.rs")
s = p.read_text()
if "pub(super) fn force_floating_geometry(" not in s:
    marker = r'''fn apply_window_state(
'''
    helper = r'''pub(super) fn force_floating_geometry(
    hwnd_value: usize,
    target: SavedRect,
) -> Result<SavedRect, String> {
    let hwnd = hwnd_value as Hwnd;
    if hwnd.is_null() {
        return Err("window handle is unavailable for floating fallback".to_owned());
    }

    // A wrong native Snap zone must be explicitly cleared before SetWindowPos;
    // otherwise Windows can retain arranged membership even when the rectangle
    // looks close to the requested fallback geometry.
    unsafe {
        ShowWindow(hwnd, SW_RESTORE);
    }
    thread::sleep(Duration::from_millis(40));
    stage_window_rect(hwnd, target)?;
    thread::sleep(Duration::from_millis(PLACEMENT_SETTLE_BASE_MS));

    let observed = observe_window(hwnd)
        .ok_or_else(|| "window became unavailable while verifying floating fallback".to_owned())?;
    if windows_snap::is_arranged(hwnd) == Some(true) {
        return Err("Windows kept the window in arranged state after SW_RESTORE fallback".to_owned());
    }
    Ok(observed.bounds)
}

'''
    if marker not in s:
        raise SystemExit("apply_window_state marker missing")
    s = s.replace(marker, helper + marker, 1)
    p.write_text(s)


# 4) A portrait pair now has a strict contract: accept native Snap only when both
# HWNDs match the saved halves; otherwise fall back to exact floating geometry.
p = Path("src/restore/custom_snap.rs")
s = p.read_text()
if "const PORTRAIT_PAIR_TOLERANCE" not in s:
    s = s.replace(
        "const LAYOUT_TOLERANCE: f64 = 0.035;\n",
        "const LAYOUT_TOLERANCE: f64 = 0.035;\nconst PORTRAIT_PAIR_TOLERANCE: i64 = 3;\n",
        1,
    )

old = r'''        let top_target = normalized_target(work, top_saved.normalized);
        let bottom_target = normalized_target(work, bottom_saved.normalized);
        let already_correct = top_target
            .is_some_and(|target| rect_distance(target, top.bounds) <= 12)
            && bottom_target.is_some_and(|target| rect_distance(target, bottom.bounds) <= 12)
            && windows_snap::is_arranged(top.hwnd as Hwnd) == Some(true)
            && windows_snap::is_arranged(bottom.hwnd as Hwnd) == Some(true);
        if already_correct {
            continue;
        }

        if let Err(error) = windows_snap::restore_equal_pair(
            top.hwnd,
            bottom.hwnd,
            SplitOrientation::Stacked,
            [work.left, work.top, work.right, work.bottom],
        ) {
            report.failures.push(format!(
                "portrait stock snap restore failed for '{}' + '{}': {error}",
                top_window.title, bottom_window.title
            ));
        }
'''
new = r'''        let Some(top_target) = normalized_target(work, top_saved.normalized) else {
            report.failures.push(format!(
                "portrait stock snap restore could not calculate the saved top-half target for '{}'",
                top_window.title
            ));
            continue;
        };
        let Some(bottom_target) = normalized_target(work, bottom_saved.normalized) else {
            report.failures.push(format!(
                "portrait stock snap restore could not calculate the saved bottom-half target for '{}'",
                bottom_window.title
            ));
            continue;
        };

        if portrait_pair_is_exact_native(top.hwnd, bottom.hwnd, top_target, bottom_target) {
            continue;
        }

        let native_result = windows_snap::restore_equal_pair(
            top.hwnd,
            bottom.hwnd,
            SplitOrientation::Stacked,
            [work.left, work.top, work.right, work.bottom],
        );
        if portrait_pair_is_exact_native(top.hwnd, bottom.hwnd, top_target, bottom_target) {
            continue;
        }

        let native_detail = native_result
            .err()
            .unwrap_or_else(|| "Windows reported native Snap success, but the settled pair did not match the saved 50/50 halves".to_owned());

        match fallback_portrait_pair_to_geometry(
            top.hwnd,
            bottom.hwnd,
            top_target,
            bottom_target,
        ) {
            Ok(()) => report.warnings.push(format!(
                "portrait native Snap was not exact for '{}' + '{}' ({native_detail}); retained the saved top/bottom halves as exact floating geometry instead",
                top_window.title, bottom_window.title
            )),
            Err(fallback_error) => report.failures.push(format!(
                "portrait restore failed for '{}' + '{}': native attempt: {native_detail}; floating fallback: {fallback_error}",
                top_window.title, bottom_window.title
            )),
        }
'''
if old in s:
    s = s.replace(old, new, 1)
elif "fallback_portrait_pair_to_geometry(" not in s:
    raise SystemExit("portrait restore block marker missing")

if "fn portrait_pair_is_exact_native(" not in s:
    marker = "fn portrait_stacked_pair_indices(desktop: &SavedDesktop) -> Vec<SavedStockPairIndex> {\n"
    helper = r'''fn portrait_pair_is_exact_native(
    top_hwnd: usize,
    bottom_hwnd: usize,
    top_target: SavedRect,
    bottom_target: SavedRect,
) -> bool {
    windows_snap::is_arranged(top_hwnd as Hwnd) == Some(true)
        && windows_snap::is_arranged(bottom_hwnd as Hwnd) == Some(true)
        && window_bounds(top_hwnd as Hwnd)
            .is_some_and(|bounds| rect_distance(bounds, top_target) <= PORTRAIT_PAIR_TOLERANCE)
        && window_bounds(bottom_hwnd as Hwnd)
            .is_some_and(|bounds| rect_distance(bounds, bottom_target) <= PORTRAIT_PAIR_TOLERANCE)
}

fn fallback_portrait_pair_to_geometry(
    top_hwnd: usize,
    bottom_hwnd: usize,
    top_target: SavedRect,
    bottom_target: SavedRect,
) -> Result<(), String> {
    let top_observed = super::windows::force_floating_geometry(top_hwnd, top_target)?;
    let bottom_observed = super::windows::force_floating_geometry(bottom_hwnd, bottom_target)?;

    if rect_distance(top_observed, top_target) > PORTRAIT_PAIR_TOLERANCE {
        return Err(format!(
            "top floating fallback missed the saved half: observed left={} top={} right={} bottom={}, target left={} top={} right={} bottom={}",
            top_observed.left,
            top_observed.top,
            top_observed.right,
            top_observed.bottom,
            top_target.left,
            top_target.top,
            top_target.right,
            top_target.bottom,
        ));
    }
    if rect_distance(bottom_observed, bottom_target) > PORTRAIT_PAIR_TOLERANCE {
        return Err(format!(
            "bottom floating fallback missed the saved half: observed left={} top={} right={} bottom={}, target left={} top={} right={} bottom={}",
            bottom_observed.left,
            bottom_observed.top,
            bottom_observed.right,
            bottom_observed.bottom,
            bottom_target.left,
            bottom_target.top,
            bottom_target.right,
            bottom_target.bottom,
        ));
    }
    Ok(())
}

'''
    if marker not in s:
        raise SystemExit("portrait pair index marker missing")
    s = s.replace(marker, helper + marker, 1)
p.write_text(s)


# 5) Decide portrait-pair membership from the FULL desktop before semantic-owned
# hosts (notably Windows Terminal) are filtered out of the prerequisite pass.
p = Path("src/restore/mod.rs")
s = p.read_text()
old = r'''            let prerequisite = desktop_without_semantic_owned_hosts(
                &desktop,
                defer_windows_terminal,
                saved_devhost,
                zen_semantic_owner,
            );
'''
new = r'''            // Determine portrait pair membership before filtering semantic-owned
            // hosts. Otherwise a VS Code top-half + deferred Windows Terminal
            // bottom-half pair is temporarily broken and VS Code gets snapped by
            // itself in this prerequisite pass, only to be processed again later.
            let portrait_geometry = custom_snap::geometry_only_portrait_stacked_pairs(&desktop);
            let prerequisite = desktop_without_semantic_owned_hosts(
                &portrait_geometry,
                defer_windows_terminal,
                saved_devhost,
                zen_semantic_owner,
            );
'''
if old in s:
    s = s.replace(old, new, 1)
elif "let portrait_geometry = custom_snap::geometry_only_portrait_stacked_pairs(&desktop);" not in s:
    raise SystemExit("prerequisite full-desktop portrait marker missing")

if "portrait_pair_geometry_is_decided_before_terminal_is_deferred" not in s:
    marker = r'''    #[test]
    fn windows_terminal_is_deferred_only_when_a_safe_terminal_plan_exists() {
'''
    test = r'''    #[cfg(windows)]
    #[test]
    fn portrait_pair_geometry_is_decided_before_terminal_is_deferred() {
        let mut code = application(
            "Visual Studio Code",
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
        );
        let mut terminal = application(
            "Windows Terminal",
            Some(r"C:\Program Files\WindowsApps\Microsoft.WindowsTerminal\WindowsTerminal.exe"),
        );

        let mut top = physical_test_window("workspace - Visual Studio Code");
        top.state = "snapped:top-half".to_owned();
        top.display_device = "DISPLAY1".to_owned();
        top.display_relation = "primary".to_owned();
        top.bounds = SavedRect {
            left: 0,
            top: 0,
            right: 1080,
            bottom: 960,
        };
        top.normalized_bounds = Some(SavedNormalizedRect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 0.5,
        });

        let mut bottom = physical_test_window("Terminal");
        bottom.state = "snapped:bottom-half".to_owned();
        bottom.display_device = "DISPLAY1".to_owned();
        bottom.display_relation = "primary".to_owned();
        bottom.bounds = SavedRect {
            left: 0,
            top: 960,
            right: 1080,
            bottom: 1920,
        };
        bottom.normalized_bounds = Some(SavedNormalizedRect {
            x: 0.0,
            y: 0.5,
            width: 1.0,
            height: 0.5,
        });
        code.windows = vec![top];
        terminal.windows = vec![bottom];

        let desktop = SavedDesktop {
            status: "available".to_owned(),
            displays: vec![SavedDisplay {
                device_name: "DISPLAY1".to_owned(),
                bounds: SavedRect {
                    left: 0,
                    top: 0,
                    right: 1080,
                    bottom: 1920,
                },
                work_area: SavedRect {
                    left: 0,
                    top: 0,
                    right: 1080,
                    bottom: 1920,
                },
                is_primary: true,
                scale_percent: 100,
                orientation: "portrait".to_owned(),
                relation_to_primary: "primary".to_owned(),
            }],
            applications: vec![code, terminal],
        };

        let portrait_geometry = custom_snap::geometry_only_portrait_stacked_pairs(&desktop);
        let prerequisite =
            desktop_without_semantic_owned_hosts(&portrait_geometry, true, false, false);

        assert_eq!(prerequisite.applications.len(), 1);
        assert_eq!(prerequisite.applications[0].name, "Visual Studio Code");
        assert_eq!(prerequisite.applications[0].windows[0].state, "normal");
        assert_eq!(desktop.applications[0].windows[0].state, "snapped:top-half");
        assert_eq!(
            desktop.applications[1].windows[0].state,
            "snapped:bottom-half"
        );
    }

'''
    if marker not in s:
        raise SystemExit("restore mod terminal test marker missing")
    s = s.replace(marker, test + marker, 1)
p.write_text(s)


# 6) Live regression validates the final user-visible contract: exact halves
# after settling. Native Snap is preferred, but exact floating fallback is valid
# and must leave both windows consistently unarranged rather than overlapped.
p = Path("tests/windows_snap_live.rs")
s = p.read_text()
s = s.replace(
    "fn live_restore_portrait_top_bottom_as_one_native_pair() {",
    "fn live_restore_portrait_top_bottom_prefers_native_or_falls_back_exactly() {",
    1,
)
old = r'''    assert_ne!(
        unsafe { IsWindowArranged(host.windows[0].hwnd()) },
        0,
        "portrait top window is only floating at the target rectangle"
    );
    assert_ne!(
        unsafe { IsWindowArranged(host.windows[1].hwnd()) },
        0,
        "portrait bottom window is only floating at the target rectangle"
    );
    assert!(
        rect_close_px(top.into(), top_target, 3),
        "portrait top geometry missed its strict target: observed={top:?}, target={top_target:?}"
    );
'''
new = r'''    let top_arranged = unsafe { IsWindowArranged(host.windows[0].hwnd()) } != 0;
    let bottom_arranged = unsafe { IsWindowArranged(host.windows[1].hwnd()) } != 0;
    assert_eq!(
        top_arranged, bottom_arranged,
        "portrait pair ended in a mixed native/floating state instead of one coherent layout"
    );
    assert!(
        rect_close_px(top.into(), top_target, 3),
        "portrait top geometry missed its strict target: observed={top:?}, target={top_target:?}"
    );
'''
if old in s:
    s = s.replace(old, new, 1)
elif "let top_arranged = unsafe" not in s:
    raise SystemExit("live portrait arranged assertions marker missing")

if "portrait-stacked-state.txt" not in s:
    marker = '    screenshot(&out_dir, "portrait-stacked-after-1200ms.png");\n'
    addition = r'''    fs::write(
        out_dir.join("portrait-stacked-state.txt"),
        format!(
            "top_arranged={top_arranged}\nbottom_arranged={bottom_arranged}\ntop={top:?}\nbottom={bottom:?}\ntop_target={top_target:?}\nbottom_target={bottom_target:?}\nfallback_warning={}\n",
            report
                .desktop
                .warnings
                .iter()
                .chain(report.warnings.iter())
                .any(|warning| warning.contains("exact floating geometry instead"))
        ),
    )
    .expect("write portrait settled state");
'''
    if marker not in s:
        raise SystemExit("live screenshot marker missing")
    s = s.replace(marker, addition + marker, 1)
p.write_text(s)

print("portrait pair final patch applied")
