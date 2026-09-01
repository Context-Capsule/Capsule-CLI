from pathlib import Path

path = Path("src/windows_snap.rs")
text = path.read_text(encoding="utf-8")
old = '''fn send_win_arrow_path(arrows: &[u16]) -> Result<(), String> {
    for (index, arrow) in arrows.iter().enumerate() {
        send_chord(&[VK_LWIN], *arrow)?;
        if index + 1 < arrows.len() {
            thread::sleep(SNAP_PATH_STEP_SETTLE);
        }
    }
    Ok(())
}
'''
new = '''fn send_win_arrow_path(arrows: &[u16]) -> Result<(), String> {
    if arrows.is_empty() {
        return Ok(());
    }

    // Quarter Snap is a single Windows-key gesture. Releasing Win after the
    // horizontal arrow commits a half-screen Snap; on current Windows 11 the
    // following independent Win+Up/Down then leaves that half unchanged. Keep
    // Win held while transitioning to the vertical arrow so Explorer receives
    // the intended corner-Snap state machine sequence.
    let mut inputs = Vec::with_capacity(arrows.len() * 2 + 2);
    inputs.push(keyboard_input(VK_LWIN, false));
    for arrow in arrows {
        inputs.push(keyboard_input(*arrow, false));
        inputs.push(keyboard_input(*arrow, true));
    }
    inputs.push(keyboard_input(VK_LWIN, true));

    let expected = inputs.len() as u32;
    let sent = unsafe { SendInput(expected, inputs.as_ptr(), size_of::<NativeInput>() as i32) };
    if sent == expected {
        thread::sleep(SNAP_PATH_STEP_SETTLE);
        return Ok(());
    }

    // Never leave a modifier synthetically held after a partial SendInput.
    let release = keyboard_input(VK_LWIN, true);
    unsafe {
        SendInput(1, &release, size_of::<NativeInput>() as i32);
    }
    Err(format!(
        "Windows accepted {sent}/{expected} native quarter-snap key events (possibly blocked by UIPI or another input policy)"
    ))
}
'''
if old not in text:
    raise SystemExit("quarter Snap sequence fragment not found")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
print("Patched quarter Snap sequence to hold Win across arrows")
