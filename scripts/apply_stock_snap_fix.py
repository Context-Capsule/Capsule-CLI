from pathlib import Path


def replace_required(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"required source fragment not found: {label}")
    return text.replace(old, new, 1)


snap_path = Path("src/windows_snap.rs")
snap = snap_path.read_text(encoding="utf-8")

snap = replace_required(
    snap,
    """const VK_LEFT: u16 = 0x25;\nconst VK_UP: u16 = 0x26;\nconst VK_RIGHT: u16 = 0x27;\nconst VK_DOWN: u16 = 0x28;\nconst VK_MENU: u16 = 0x12;\nconst VK_LWIN: u16 = 0x5B;\n""",
    """const VK_SHIFT: u16 = 0x10;\nconst VK_CONTROL: u16 = 0x11;\nconst VK_MENU: u16 = 0x12;\nconst VK_ESCAPE: u16 = 0x1B;\nconst VK_LEFT: u16 = 0x25;\nconst VK_UP: u16 = 0x26;\nconst VK_RIGHT: u16 = 0x27;\nconst VK_DOWN: u16 = 0x28;\nconst VK_LWIN: u16 = 0x5B;\nconst VK_Z: u16 = 0x5A;\n""",
    "snap virtual keys",
)

snap = replace_required(
    snap,
    """const SNAP_SETTLE: Duration = Duration::from_millis(220);\n""",
    """const SNAP_SETTLE: Duration = Duration::from_millis(220);\nconst SNAP_PATH_STEP_SETTLE: Duration = Duration::from_millis(120);\nconst SNAP_LAYOUT_OPEN_SETTLE: Duration = Duration::from_millis(280);\nconst SNAP_LAYOUT_SELECT_SETTLE: Duration = Duration::from_millis(220);\nconst SNAP_LAYOUT_RESULT_TIMEOUT: Duration = Duration::from_millis(1_300);\nconst SNAP_LAYOUT_DISMISS_SETTLE: Duration = Duration::from_millis(90);\n""",
    "snap timing constants",
)

snap = replace_required(
    snap,
    """pub(crate) enum SnapDirection {\n    LeftHalf,\n    RightHalf,\n    TopHalf,\n    BottomHalf,\n}\n""",
    """pub(crate) enum SnapDirection {\n    LeftHalf,\n    RightHalf,\n    TopHalf,\n    BottomHalf,\n    TopLeftQuarter,\n    TopRightQuarter,\n    BottomLeftQuarter,\n    BottomRightQuarter,\n    LeftThird,\n    CenterThird,\n    RightThird,\n    LeftTwoThirds,\n    RightTwoThirds,\n}\n""",
    "SnapDirection variants",
)

snap = replace_required(
    snap,
    """    fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;\n    fn AttachThreadInput(id_attach: u32, id_attach_to: u32, attach: Bool) -> Bool;\n    fn SendInput(count: u32, inputs: *const NativeInput, size: i32) -> u32;\n""",
    """    fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;\n    fn GetKeyboardLayout(thread_id: u32) -> Handle;\n    fn VkKeyScanExW(character: u16, keyboard_layout: Handle) -> i16;\n    fn AttachThreadInput(id_attach: u32, id_attach_to: u32, attach: Bool) -> Bool;\n    fn SendInput(count: u32, inputs: *const NativeInput, size: i32) -> u32;\n""",
    "keyboard-layout APIs",
)

old_snap = """pub(crate) fn snap(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {\n    if hwnd.is_null() {\n        return Err(\"window handle is unavailable\".to_owned());\n    }\n    if is_arranged(hwnd).is_none() {\n        return Err(\n            \"Windows does not expose IsWindowArranged; refusing to inject a snap shortcut without post-action verification\"\n                .to_owned(),\n        );\n    }\n\n    if !focus_window_without_geometry_change(hwnd) {\n        return Err(\n            \"Windows foreground-lock policy prevented focusing the intended window for native snap\"\n                .to_owned(),\n        );\n    }\n\n    let (modifiers, arrow): (&[u16], u16) = match direction {\n        SnapDirection::LeftHalf => (&[VK_LWIN], VK_LEFT),\n        SnapDirection::RightHalf => (&[VK_LWIN], VK_RIGHT),\n        SnapDirection::TopHalf => (&[VK_LWIN, VK_MENU], VK_UP),\n        SnapDirection::BottomHalf => (&[VK_LWIN, VK_MENU], VK_DOWN),\n    };\n    send_chord(modifiers, arrow)?;\n    thread::sleep(SNAP_SETTLE);\n    Ok(is_arranged(hwnd).unwrap_or(false))\n}\n"""

new_snap = """pub(crate) fn snap(hwnd: Hwnd, direction: SnapDirection) -> Result<bool, String> {\n    if hwnd.is_null() {\n        return Err(\"window handle is unavailable\".to_owned());\n    }\n    if is_arranged(hwnd).is_none() {\n        return Err(\n            \"Windows does not expose IsWindowArranged; refusing to inject a snap shortcut without post-action verification\"\n                .to_owned(),\n        );\n    }\n\n    if !focus_window_without_geometry_change(hwnd) {\n        return Err(\n            \"Windows foreground-lock policy prevented focusing the intended window for native snap\"\n                .to_owned(),\n        );\n    }\n\n    match direction {\n        SnapDirection::LeftHalf => send_chord(&[VK_LWIN], VK_LEFT)?,\n        SnapDirection::RightHalf => send_chord(&[VK_LWIN], VK_RIGHT)?,\n        SnapDirection::TopHalf => send_chord(&[VK_LWIN, VK_MENU], VK_UP)?,\n        SnapDirection::BottomHalf => send_chord(&[VK_LWIN, VK_MENU], VK_DOWN)?,\n        SnapDirection::TopLeftQuarter => send_win_arrow_path(&[VK_LEFT, VK_UP])?,\n        SnapDirection::TopRightQuarter => send_win_arrow_path(&[VK_RIGHT, VK_UP])?,\n        SnapDirection::BottomLeftQuarter => send_win_arrow_path(&[VK_LEFT, VK_DOWN])?,\n        SnapDirection::BottomRightQuarter => send_win_arrow_path(&[VK_RIGHT, VK_DOWN])?,\n        direction => {\n            let (layout, zone) = snap_layout_choice(direction).ok_or_else(|| {\n                format!(\"no native Snap Layout mapping is available for {direction:?}\")\n            })?;\n            return snap_layout_zone(hwnd, layout, zone);\n        }\n    }\n\n    thread::sleep(SNAP_SETTLE);\n    Ok(is_arranged(hwnd).unwrap_or(false))\n}\n\nfn snap_layout_choice(direction: SnapDirection) -> Option<(u8, u8)> {\n    Some(match direction {\n        // Layout 3 is Windows' three equal vertical columns template. Using the\n        // same template for all one-third slots preserves native thirds instead\n        // of emulating those rectangles with SetWindowPos.\n        SnapDirection::LeftThird => (3, 1),\n        SnapDirection::CenterThird => (3, 2),\n        SnapDirection::RightThird => (3, 3),\n        // Layout 2 is 2/3 + 1/3 and layout 4 is the mirrored 1/3 + 2/3.\n        SnapDirection::LeftTwoThirds => (2, 1),\n        SnapDirection::RightTwoThirds => (4, 2),\n        _ => return None,\n    })\n}\n\nfn snap_layout_zone(hwnd: Hwnd, layout: u8, zone: u8) -> Result<bool, String> {\n    if !(1..=9).contains(&layout) || !(1..=9).contains(&zone) {\n        return Err(format!(\"invalid Snap Layout access key {layout}:{zone}\"));\n    }\n\n    // Win+Z operates only on the foreground HWND. The caller has already\n    // verified focus, and we refuse to send any access key if that invariant\n    // has changed. This path intentionally uses the real target window only;\n    // it never creates a visible helper/probe HWND.\n    if unsafe { GetForegroundWindow() } != hwnd {\n        return Err(\n            \"the native Snap Layout target lost foreground focus before Win+Z; no layout keys were sent\"\n                .to_owned(),\n        );\n    }\n\n    send_chord(&[VK_LWIN], VK_Z)?;\n    thread::sleep(SNAP_LAYOUT_OPEN_SETTLE);\n\n    let result = (|| {\n        if unsafe { GetForegroundWindow() } != hwnd {\n            return Err(\n                \"the Snap Layout flyout opened for a different foreground window; refusing to continue\"\n                    .to_owned(),\n            );\n        }\n        send_access_digit(hwnd, layout)?;\n        thread::sleep(SNAP_LAYOUT_SELECT_SETTLE);\n        send_access_digit(hwnd, zone)?;\n\n        let deadline = Instant::now() + SNAP_LAYOUT_RESULT_TIMEOUT;\n        while Instant::now() < deadline {\n            if is_arranged(hwnd) == Some(true) {\n                return Ok(true);\n            }\n            thread::sleep(FOREGROUND_POLL_INTERVAL);\n        }\n        Ok(false)\n    })();\n\n    // A successful zone choice invokes Snap Assist. Escape dismisses only that\n    // shell UI; it does not undo the arranged state. Also use it on failure so a\n    // half-open Win+Z flyout is never left behind as restore UI debris.\n    let _ = send_chord(&[], VK_ESCAPE);\n    thread::sleep(SNAP_LAYOUT_DISMISS_SETTLE);\n\n    result.map(|arranged| arranged && is_arranged(hwnd) == Some(true))\n}\n\nfn send_access_digit(hwnd: Hwnd, digit: u8) -> Result<(), String> {\n    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };\n    if thread_id == 0 {\n        return Err(\"could not resolve the target keyboard layout for Snap Layout access keys\".to_owned());\n    }\n    let keyboard_layout = unsafe { GetKeyboardLayout(thread_id) };\n    let character = u16::from(b'0' + digit);\n    let mapping = unsafe { VkKeyScanExW(character, keyboard_layout) };\n    if mapping == -1 {\n        return Err(format!(\n            \"the target keyboard layout cannot generate Snap Layout digit '{digit}'\"\n        ));\n    }\n\n    let virtual_key = (mapping as u16) & 0x00ff;\n    let shift_state = ((mapping as u16) >> 8) & 0x00ff;\n    let mut modifiers = Vec::with_capacity(3);\n    if shift_state & 0x01 != 0 {\n        modifiers.push(VK_SHIFT);\n    }\n    if shift_state & 0x02 != 0 {\n        modifiers.push(VK_CONTROL);\n    }\n    if shift_state & 0x04 != 0 {\n        modifiers.push(VK_MENU);\n    }\n    send_chord(&modifiers, virtual_key)\n}\n\nfn send_win_arrow_path(arrows: &[u16]) -> Result<(), String> {\n    if arrows.is_empty() {\n        return Ok(());\n    }\n\n    let mut inputs = Vec::with_capacity(arrows.len() * 2 + 2);\n    inputs.push(keyboard_input(VK_LWIN, false));\n    for arrow in arrows {\n        inputs.push(keyboard_input(*arrow, false));\n        inputs.push(keyboard_input(*arrow, true));\n    }\n    inputs.push(keyboard_input(VK_LWIN, true));\n\n    let expected = inputs.len() as u32;\n    let sent = unsafe { SendInput(expected, inputs.as_ptr(), size_of::<NativeInput>() as i32) };\n    if sent != expected {\n        let releases = [keyboard_input(VK_LWIN, true)];\n        unsafe {\n            SendInput(\n                releases.len() as u32,\n                releases.as_ptr(),\n                size_of::<NativeInput>() as i32,\n            );\n        }\n        return Err(format!(\n            \"Windows accepted {sent}/{expected} native quarter-snap key events\"\n        ));\n    }\n\n    if arrows.len() > 1 {\n        thread::sleep(SNAP_PATH_STEP_SETTLE);\n    }\n    Ok(())\n}\n"""

snap = replace_required(snap, old_snap, new_snap, "core snap implementation")

snap = replace_required(
    snap,
    """    #[test]\n    fn arrangement_hint_is_consumed_once() {\n""",
    """    #[test]\n    fn stock_snap_layout_choices_cover_all_third_variants() {\n        assert_eq!(snap_layout_choice(SnapDirection::LeftThird), Some((3, 1)));\n        assert_eq!(snap_layout_choice(SnapDirection::CenterThird), Some((3, 2)));\n        assert_eq!(snap_layout_choice(SnapDirection::RightThird), Some((3, 3)));\n        assert_eq!(snap_layout_choice(SnapDirection::LeftTwoThirds), Some((2, 1)));\n        assert_eq!(snap_layout_choice(SnapDirection::RightTwoThirds), Some((4, 2)));\n        assert_eq!(snap_layout_choice(SnapDirection::TopLeftQuarter), None);\n    }\n\n    #[test]\n    fn arrangement_hint_is_consumed_once() {\n""",
    "stock Snap mapping unit test",
)

snap_path.write_text(snap, encoding="utf-8")

restore_path = Path("src/restore/windows.rs")
restore = restore_path.read_text(encoding="utf-8")
restore = replace_required(
    restore,
    """fn native_snap_direction(slot: SnapSlot) -> Option<SnapDirection> {\n    match slot {\n        SnapSlot::LeftHalf => Some(SnapDirection::LeftHalf),\n        SnapSlot::RightHalf => Some(SnapDirection::RightHalf),\n        SnapSlot::TopHalf => Some(SnapDirection::TopHalf),\n        SnapSlot::BottomHalf => Some(SnapDirection::BottomHalf),\n        SnapSlot::TopLeftQuarter\n        | SnapSlot::TopRightQuarter\n        | SnapSlot::BottomLeftQuarter\n        | SnapSlot::BottomRightQuarter\n        | SnapSlot::LeftThird\n        | SnapSlot::CenterThird\n        | SnapSlot::RightThird\n        | SnapSlot::LeftTwoThirds\n        | SnapSlot::RightTwoThirds => None,\n    }\n}\n""",
    """fn native_snap_direction(slot: SnapSlot) -> Option<SnapDirection> {\n    Some(match slot {\n        SnapSlot::LeftHalf => SnapDirection::LeftHalf,\n        SnapSlot::RightHalf => SnapDirection::RightHalf,\n        SnapSlot::TopHalf => SnapDirection::TopHalf,\n        SnapSlot::BottomHalf => SnapDirection::BottomHalf,\n        SnapSlot::TopLeftQuarter => SnapDirection::TopLeftQuarter,\n        SnapSlot::TopRightQuarter => SnapDirection::TopRightQuarter,\n        SnapSlot::BottomLeftQuarter => SnapDirection::BottomLeftQuarter,\n        SnapSlot::BottomRightQuarter => SnapDirection::BottomRightQuarter,\n        SnapSlot::LeftThird => SnapDirection::LeftThird,\n        SnapSlot::CenterThird => SnapDirection::CenterThird,\n        SnapSlot::RightThird => SnapDirection::RightThird,\n        SnapSlot::LeftTwoThirds => SnapDirection::LeftTwoThirds,\n        SnapSlot::RightTwoThirds => SnapDirection::RightTwoThirds,\n    })\n}\n""",
    "restore stock Snap mapping",
)
restore = replace_required(
    restore,
    """    if native_snap_direction(slot).is_none() {\n        return true;\n    }\n\n    windows_snap::is_arranged(current.hwnd as Hwnd).unwrap_or(true)\n""",
    """    if native_snap_direction(slot).is_none() {\n        return true;\n    }\n\n    windows_snap::is_arranged(current.hwnd as Hwnd) == Some(true)\n""",
    "strict arranged-state verification",
)
restore_path.write_text(restore, encoding="utf-8")

print("Applied stock native Snap coverage patch")
