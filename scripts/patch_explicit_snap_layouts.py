from pathlib import Path


def replace_required(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"required source fragment not found: {label}")
    return text.replace(old, new, 1)


core_path = Path("src/windows_snap.rs")
core = core_path.read_text(encoding="utf-8")

core = replace_required(
    core,
    "const SNAP_PATH_STEP_SETTLE: Duration = Duration::from_millis(180);\n",
    "",
    "quarter path timing constant",
)

core = replace_required(
    core,
    """        SnapDirection::TopLeftQuarter => send_win_arrow_path(&[VK_LEFT, VK_UP])?,
        SnapDirection::TopRightQuarter => send_win_arrow_path(&[VK_RIGHT, VK_UP])?,
        SnapDirection::BottomLeftQuarter => send_win_arrow_path(&[VK_LEFT, VK_DOWN])?,
        SnapDirection::BottomRightQuarter => send_win_arrow_path(&[VK_RIGHT, VK_DOWN])?,
        direction => {
""",
    """        direction => {
""",
    "quarter match arms",
)

core = replace_required(
    core,
    """    Some(match direction {
        // Windows 11 numbers the standard landscape templates in the Win+Z
        // flyout as: 1=halves, 2=2/3+1/3, 3=1/3+2/3, 4=three thirds,
        // 5=half+two quarters, 6=four quarters. A one-third slot is chosen from
        // an asymmetric two-zone template when possible so a paired 2/3 window
        // can share the same native template; center-third necessarily uses 4.
        SnapDirection::LeftThird => (3, 1),
""",
    """    Some(match direction {
        // Windows 11 numbers the standard landscape templates in the Win+Z
        // flyout as: 1=halves, 2=2/3+1/3, 3=1/3+2/3, 4=three thirds,
        // 5=half+two quarters, 6=four quarters. Use explicit layout+zone access
        // keys for every corner/third slot so restore does not depend on the
        // previous arranged state of the target window.
        SnapDirection::TopLeftQuarter => (6, 1),
        SnapDirection::TopRightQuarter => (6, 2),
        SnapDirection::BottomLeftQuarter => (6, 3),
        SnapDirection::BottomRightQuarter => (6, 4),
        SnapDirection::LeftThird => (3, 1),
""",
    "explicit quarter layout choices",
)

start = core.find("fn send_win_arrow_path(arrows: &[u16]) -> Result<(), String> {")
if start == -1:
    raise SystemExit("quarter arrow helper not found")
end_marker = "\nfn focus_window_without_geometry_change"
end = core.find(end_marker, start)
if end == -1:
    raise SystemExit("quarter arrow helper end marker not found")
core = core[:start] + core[end + 1 :]

core = replace_required(
    core,
    "        assert_eq!(snap_layout_choice(SnapDirection::TopLeftQuarter), None);\n",
    """        assert_eq!(snap_layout_choice(SnapDirection::TopLeftQuarter), Some((6, 1)));
        assert_eq!(snap_layout_choice(SnapDirection::TopRightQuarter), Some((6, 2)));
        assert_eq!(snap_layout_choice(SnapDirection::BottomLeftQuarter), Some((6, 3)));
        assert_eq!(snap_layout_choice(SnapDirection::BottomRightQuarter), Some((6, 4)));
""",
    "quarter mapping tests",
)

core_path.write_text(core, encoding="utf-8")

baseline_path = Path("src/windows_snap_baseline.rs")
baseline = baseline_path.read_text(encoding="utf-8")

baseline = replace_required(
    baseline,
    """pub(crate) fn snap(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
    let mut failures = Vec::new();
""",
    """pub(crate) fn snap(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
    if uses_explicit_layout_zone(direction) {
        return snap_explicit_layout_zone(hwnd, direction);
    }

    let mut failures = Vec::new();
""",
    "explicit layout fast path",
)

insert_before = "fn prepare_floating_baseline(hwnd: Hwnd) -> Result<[i32; 4], String> {"
idx = baseline.find(insert_before)
if idx == -1:
    raise SystemExit("floating baseline function marker not found")
helper = '''fn uses_explicit_layout_zone(direction: SnapDirection) -> bool {
    matches!(
        direction,
        SnapDirection::TopLeftQuarter
            | SnapDirection::TopRightQuarter
            | SnapDirection::BottomLeftQuarter
            | SnapDirection::BottomRightQuarter
            | SnapDirection::LeftThird
            | SnapDirection::CenterThird
            | SnapDirection::RightThird
            | SnapDirection::LeftTwoThirds
            | SnapDirection::RightTwoThirds
    )
}

fn snap_explicit_layout_zone(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {
    if hwnd.is_null() {
        return Err("window handle is unavailable".to_owned());
    }

    // The generic restore pass has already staged the target frame on the saved
    // display. An explicit Win+Z layout/zone selection is absolute within that
    // display and does not depend on the target's previous Snap state, so trying
    // to force it through SW_RESTORE first is unnecessary and, for some Snap
    // groups, incorrect: Windows can keep IsWindowArranged=true after programmatic
    // SW_RESTORE/SetWindowPos even though a new explicit zone remains selectable.
    let expected_work = target_work_area(hwnd)?;
    let before = monitor_work_area(hwnd).ok_or_else(|| {
        "could not determine the target monitor before explicit Snap Layout selection".to_owned()
    })?;
    if !same_work_area(before, expected_work) {
        return Err(format!(
            "explicit Snap Layout target is on work area {:?} instead of intended {:?}",
            before, expected_work
        ));
    }

    let arranged = windows_snap_core::snap(hwnd, direction)?;
    let after = monitor_work_area(hwnd).ok_or_else(|| {
        "could not determine the target monitor after explicit Snap Layout selection".to_owned()
    })?;
    if arranged && same_work_area(after, expected_work) {
        return Ok(true);
    }

    Err(format!(
        "explicit Snap Layout selection did not produce a verified arranged window on the intended monitor: arranged={arranged}, expected work area {:?}, observed {:?}",
        expected_work, after
    ))
}

'''
baseline = baseline[:idx] + helper + baseline[idx:]

baseline = replace_required(
    baseline,
    """    #[test]
    fn target_work_area_scope_restores_previous_value() {
""",
    """    #[test]
    fn explicit_layout_slots_do_not_require_stateful_floating_baseline() {
        for direction in [
            SnapDirection::TopLeftQuarter,
            SnapDirection::TopRightQuarter,
            SnapDirection::BottomLeftQuarter,
            SnapDirection::BottomRightQuarter,
            SnapDirection::LeftThird,
            SnapDirection::CenterThird,
            SnapDirection::RightThird,
            SnapDirection::LeftTwoThirds,
            SnapDirection::RightTwoThirds,
        ] {
            assert!(uses_explicit_layout_zone(direction));
        }
        assert!(!uses_explicit_layout_zone(SnapDirection::LeftHalf));
        assert!(!uses_explicit_layout_zone(SnapDirection::RightHalf));
        assert!(!uses_explicit_layout_zone(SnapDirection::TopHalf));
        assert!(!uses_explicit_layout_zone(SnapDirection::BottomHalf));
    }

    #[test]
    fn target_work_area_scope_restores_previous_value() {
""",
    "explicit layout baseline test",
)

baseline_path.write_text(baseline, encoding="utf-8")
print("Patched explicit Snap Layout zones to bypass stateful floating baseline")
