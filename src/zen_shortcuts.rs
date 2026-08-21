#![cfg(windows)]

use serde::Deserialize;
use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

const INPUT_KEYBOARD: u32 = 1;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
const VK_LWIN: u16 = 0x5B;
const FOREGROUND_RETRIES: usize = 8;
const FOREGROUND_SETTLE: Duration = Duration::from_millis(60);
const SHORTCUT_STEP_SETTLE: Duration = Duration::from_millis(12);

#[repr(C)]
#[derive(Clone, Copy)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KeyboardInput {
    virtual_key: u16,
    scan_code: u16,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct HardwareInput {
    message: u32,
    param_l: u16,
    param_h: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
union InputData {
    mouse: MouseInput,
    keyboard: KeyboardInput,
    hardware: HardwareInput,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Input {
    kind: u32,
    data: InputData,
}

type Hwnd = *mut core::ffi::c_void;
type Handle = *mut core::ffi::c_void;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetForegroundWindow() -> Hwnd;
    fn GetWindowThreadProcessId(window: Hwnd, process_id: *mut u32) -> u32;
    fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
    fn VkKeyScanW(character: u16) -> i16;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit_handle: i32, process_id: u32) -> Handle;
    fn QueryFullProcessImageNameW(
        process: Handle,
        flags: u32,
        buffer: *mut u16,
        size: *mut u32,
    ) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetLastError() -> u32;
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct ShortcutModifiers {
    #[serde(default)]
    control: bool,
    #[serde(default)]
    alt: bool,
    #[serde(default)]
    shift: bool,
    #[serde(default)]
    meta: bool,
    #[serde(default)]
    accel: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ShortcutBinding {
    #[serde(default)]
    key: String,
    #[serde(default)]
    keycode: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    modifiers: ShortcutModifiers,
}

#[derive(Debug, Deserialize)]
struct ShortcutFile {
    #[serde(default)]
    shortcuts: Vec<ShortcutBinding>,
}

#[derive(Debug, Clone)]
struct ResolvedShortcut {
    virtual_key: u16,
    modifiers: ShortcutModifiers,
    source: &'static str,
}

#[derive(Debug, Clone)]
struct ProfileEntry {
    path: String,
    relative: bool,
    default: bool,
}

pub(crate) fn invoke_split_shortcut(orientation: &str) -> Result<(), String> {
    let (action, fallback_key) = split_action(orientation)?;
    wait_for_foreground_zen()?;
    let shortcut = resolve_shortcut(action, fallback_key)?;
    send_shortcut(&shortcut)?;
    Ok(())
}

fn split_action(orientation: &str) -> Result<(&'static str, char), String> {
    match orientation.trim().to_ascii_lowercase().as_str() {
        "vertical" => Ok(("cmd_zenSplitViewVertical", 'V')),
        "horizontal" => Ok(("cmd_zenSplitViewHorizontal", 'H')),
        "grid" => Ok(("cmd_zenSplitViewGrid", 'G')),
        other => Err(format!(
            "unsupported Zen split orientation '{other}'; expected vertical, horizontal, or grid"
        )),
    }
}

fn resolve_shortcut(action: &str, fallback_key: char) -> Result<ResolvedShortcut, String> {
    if let Some(path) = zen_shortcuts_path()? {
        if path.is_file() {
            let bytes = fs::read(&path)
                .map_err(|error| format!("could not read Zen shortcuts '{}': {error}", path.display()))?;
            let parsed: ShortcutFile = serde_json::from_slice(&bytes)
                .map_err(|error| format!("Zen shortcuts '{}' are invalid JSON: {error}", path.display()))?;
            let binding = parsed.shortcuts.iter().find(|binding| binding.action == action);
            if let Some(binding) = binding {
                if binding.disabled {
                    return Err(format!(
                        "Zen shortcut for {action} is disabled in '{}'; Context Capsule will not guess another key binding",
                        path.display()
                    ));
                }
                let virtual_key = binding_virtual_key(binding)?;
                return Ok(ResolvedShortcut {
                    virtual_key,
                    modifiers: binding.modifiers,
                    source: "Zen profile",
                });
            }
            return Err(format!(
                "Zen shortcut action {action} is missing from '{}'; Context Capsule will not send an unverified default binding",
                path.display()
            ));
        }
    }

    Ok(ResolvedShortcut {
        virtual_key: ascii_virtual_key(fallback_key)?,
        modifiers: ShortcutModifiers {
            control: false,
            alt: true,
            shift: false,
            meta: false,
            accel: true,
        },
        source: "Zen default",
    })
}

fn binding_virtual_key(binding: &ShortcutBinding) -> Result<u16, String> {
    if !binding.keycode.trim().is_empty() {
        return virtual_key_from_keycode(binding.keycode.trim()).ok_or_else(|| {
            format!("unsupported Zen keycode '{}' for {}", binding.keycode, binding.action)
        });
    }
    let mut chars = binding.key.chars();
    let character = chars
        .next()
        .ok_or_else(|| format!("Zen shortcut {} has no key or keycode", binding.action))?;
    if chars.next().is_some() {
        return Err(format!(
            "Zen shortcut {} uses unsupported multi-character key '{}'",
            binding.action, binding.key
        ));
    }
    char_virtual_key(character)
}

fn ascii_virtual_key(character: char) -> Result<u16, String> {
    let upper = character.to_ascii_uppercase();
    if upper.is_ascii_alphanumeric() {
        return Ok(upper as u16);
    }
    char_virtual_key(character)
}

fn char_virtual_key(character: char) -> Result<u16, String> {
    let code = u32::from(character);
    if code > u16::MAX as u32 {
        return Err(format!("shortcut character '{character}' is outside the Windows BMP"));
    }
    let mapped = unsafe { VkKeyScanW(code as u16) };
    if mapped == -1 {
        return Err(format!("Windows cannot map Zen shortcut character '{character}' to a virtual key"));
    }
    Ok((mapped as u16) & 0x00ff)
}

fn virtual_key_from_keycode(keycode: &str) -> Option<u16> {
    let upper = keycode.trim().to_ascii_uppercase();
    if let Some(number) = upper.strip_prefix("VK_F").and_then(|value| value.parse::<u16>().ok()) {
        if (1..=24).contains(&number) {
            return Some(0x70 + number - 1);
        }
    }
    Some(match upper.as_str() {
        "VK_TAB" => 0x09,
        "VK_RETURN" | "VK_ENTER" => 0x0D,
        "VK_ESCAPE" => 0x1B,
        "VK_SPACE" => 0x20,
        "VK_PRIOR" | "VK_PAGE_UP" => 0x21,
        "VK_NEXT" | "VK_PAGE_DOWN" => 0x22,
        "VK_END" => 0x23,
        "VK_HOME" => 0x24,
        "VK_LEFT" => 0x25,
        "VK_UP" => 0x26,
        "VK_RIGHT" => 0x27,
        "VK_DOWN" => 0x28,
        "VK_INSERT" => 0x2D,
        "VK_DELETE" => 0x2E,
        "VK_BACK" | "VK_BACKSPACE" => 0x08,
        _ => return None,
    })
}

fn send_input_step(inputs: &[Input], source: &str) -> Result<(), String> {
    let count = u32::try_from(inputs.len()).map_err(|_| "too many keyboard events".to_owned())?;
    if count == 0 {
        return Ok(());
    }
    let sent = unsafe {
        SendInput(
            count,
            inputs.as_ptr(),
            i32::try_from(std::mem::size_of::<Input>()).unwrap_or(i32::MAX),
        )
    };
    if sent != count {
        let error = unsafe { GetLastError() };
        return Err(format!(
            "Zen split shortcut from {source} was not fully injected ({sent}/{count} INPUT events, Win32 error {error}). Synthetic input can be blocked by UIPI; run Zen and Context Capsule at matching elevation"
        ));
    }
    Ok(())
}

fn release_modifiers_best_effort(modifiers: &[u16]) {
    for virtual_key in modifiers.iter().rev() {
        let input = [keyboard_input(*virtual_key, true)];
        unsafe {
            SendInput(
                1,
                input.as_ptr(),
                i32::try_from(std::mem::size_of::<Input>()).unwrap_or(i32::MAX),
            );
        }
    }
}

fn send_shortcut(shortcut: &ResolvedShortcut) -> Result<(), String> {
    // On Windows Zen treats Accel as Ctrl. Keep declared Control as Ctrl too;
    // duplicate modifiers are collapsed before generating INPUT records.
    let mut modifiers = Vec::new();
    if shortcut.modifiers.control || shortcut.modifiers.accel {
        modifiers.push(VK_CONTROL);
    }
    if shortcut.modifiers.alt {
        modifiers.push(VK_MENU);
    }
    if shortcut.modifiers.shift {
        modifiers.push(VK_SHIFT);
    }
    if shortcut.modifiers.meta {
        modifiers.push(VK_LWIN);
    }

    // Deliver the chord with a short human-like cadence instead of putting the
    // entire key-down/key-up burst in one SendInput call. Gecko/Zen's chrome
    // key handlers then observe the modifier state before the command key. If a
    // step fails, release every modifier best-effort so Context Capsule never
    // leaves Ctrl/Alt/Shift/Win logically held down.
    for virtual_key in &modifiers {
        if let Err(error) = send_input_step(&[keyboard_input(*virtual_key, false)], shortcut.source) {
            release_modifiers_best_effort(&modifiers);
            return Err(error);
        }
        thread::sleep(SHORTCUT_STEP_SETTLE);
    }

    let command = [
        keyboard_input(shortcut.virtual_key, false),
        keyboard_input(shortcut.virtual_key, true),
    ];
    if let Err(error) = send_input_step(&command, shortcut.source) {
        release_modifiers_best_effort(&modifiers);
        return Err(error);
    }
    thread::sleep(SHORTCUT_STEP_SETTLE);

    for virtual_key in modifiers.iter().rev() {
        if let Err(error) = send_input_step(&[keyboard_input(*virtual_key, true)], shortcut.source) {
            release_modifiers_best_effort(&modifiers);
            return Err(error);
        }
    }
    Ok(())
}

fn keyboard_input(virtual_key: u16, key_up: bool) -> Input {
    Input {
        kind: INPUT_KEYBOARD,
        data: InputData {
            keyboard: KeyboardInput {
                virtual_key,
                scan_code: 0,
                flags: if key_up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                extra_info: 0,
            },
        },
    }
}

fn wait_for_foreground_zen() -> Result<(), String> {
    let mut last = String::from("no foreground window");
    for _ in 0..FOREGROUND_RETRIES {
        match foreground_executable() {
            Ok(Some(path)) if is_zen_path(&path) => return Ok(()),
            Ok(Some(path)) => last = path.display().to_string(),
            Ok(None) => last = String::from("no foreground process"),
            Err(error) => last = error,
        }
        thread::sleep(FOREGROUND_SETTLE);
    }
    Err(format!(
        "refusing to send Zen split shortcut because the foreground application is not zen.exe (observed: {last})"
    ))
}

fn foreground_executable() -> Result<Option<PathBuf>, String> {
    let window = unsafe { GetForegroundWindow() };
    if window.is_null() {
        return Ok(None);
    }
    let mut process_id = 0_u32;
    unsafe { GetWindowThreadProcessId(window, &mut process_id) };
    if process_id == 0 {
        return Ok(None);
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        let error = unsafe { GetLastError() };
        return Err(format!("OpenProcess({process_id}) failed with Win32 error {error}"));
    }

    let mut buffer = vec![0_u16; 32_768];
    let mut size = u32::try_from(buffer.len()).unwrap_or(32_768);
    let ok = unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut size) };
    unsafe { CloseHandle(process) };
    if ok == 0 {
        let error = unsafe { GetLastError() };
        return Err(format!("QueryFullProcessImageNameW failed with Win32 error {error}"));
    }
    buffer.truncate(size as usize);
    Ok(Some(PathBuf::from(String::from_utf16_lossy(&buffer))))
}

fn is_zen_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("zen.exe") || name.eq_ignore_ascii_case("zen"))
}

fn zen_shortcuts_path() -> Result<Option<PathBuf>, String> {
    if let Some(explicit) = env::var_os("CONTEXT_CAPSULE_ZEN_PROFILE") {
        if explicit.is_empty() {
            return Err("CONTEXT_CAPSULE_ZEN_PROFILE is empty".to_owned());
        }
        return Ok(Some(PathBuf::from(explicit).join("zen-keyboard-shortcuts.json")));
    }

    let Some(app_data) = env::var_os("APPDATA") else {
        return Ok(None);
    };
    let root = PathBuf::from(app_data).join("zen");
    let profiles_ini = root.join("profiles.ini");
    if !profiles_ini.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&profiles_ini)
        .map_err(|error| format!("could not read '{}': {error}", profiles_ini.display()))?;
    let profile = resolve_profile_from_ini(&root, &text)?;
    Ok(profile.map(|path| path.join("zen-keyboard-shortcuts.json")))
}

fn resolve_profile_from_ini(root: &Path, text: &str) -> Result<Option<PathBuf>, String> {
    let mut section = String::new();
    let mut values: HashMap<String, HashMap<String, String>> = HashMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_owned();
            values.entry(section.clone()).or_default();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        if section.is_empty() {
            continue;
        }
        values
            .entry(section.clone())
            .or_default()
            .insert(key.trim().to_owned(), value.trim().to_owned());
    }

    for (name, section_values) in &values {
        if !name.starts_with("Install") {
            continue;
        }
        if let Some(default) = section_values.get("Default").filter(|value| !value.is_empty()) {
            return Ok(Some(resolve_profile_path(root, default, true)));
        }
    }

    let mut profiles = Vec::new();
    for (name, section_values) in &values {
        if !name.starts_with("Profile") {
            continue;
        }
        let Some(path) = section_values.get("Path").filter(|value| !value.is_empty()) else {
            continue;
        };
        profiles.push(ProfileEntry {
            path: path.clone(),
            relative: section_values.get("IsRelative").is_none_or(|value| value != "0"),
            default: section_values.get("Default").is_some_and(|value| value == "1"),
        });
    }
    let selected = profiles.iter().find(|profile| profile.default).or_else(|| profiles.first());
    Ok(selected.map(|profile| resolve_profile_path(root, &profile.path, profile.relative)))
}

fn resolve_profile_path(root: &Path, value: &str, relative: bool) -> PathBuf {
    if relative {
        root.join(value.replace('/', "\\"))
    } else {
        PathBuf::from(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_zen_split_actions() {
        assert_eq!(split_action("vertical").unwrap(), ("cmd_zenSplitViewVertical", 'V'));
        assert_eq!(split_action("horizontal").unwrap(), ("cmd_zenSplitViewHorizontal", 'H'));
        assert_eq!(split_action("grid").unwrap(), ("cmd_zenSplitViewGrid", 'G'));
        assert!(split_action("diagonal").is_err());
    }

    #[test]
    fn parses_default_profile_from_install_section() {
        let root = Path::new(r"C:\Users\test\AppData\Roaming\zen");
        let ini = "[Profile0]\nName=default\nIsRelative=1\nPath=Profiles/abc.default\n\n[Install123]\nDefault=Profiles/abc.default\nLocked=1\n";
        let path = resolve_profile_from_ini(root, ini).unwrap().unwrap();
        assert!(path.to_string_lossy().ends_with(r"Profiles\abc.default"));
    }

    #[test]
    fn supports_zen_default_keycodes() {
        assert_eq!(virtual_key_from_keycode("VK_LEFT"), Some(0x25));
        assert_eq!(virtual_key_from_keycode("VK_F12"), Some(0x7B));
        assert_eq!(virtual_key_from_keycode("VK_RETURN"), Some(0x0D));
        assert_eq!(virtual_key_from_keycode("VK_NOT_REAL"), None);
    }
}
