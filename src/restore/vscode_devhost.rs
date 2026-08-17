use serde_json::Value;
use std::{
    ffi::c_void,
    path::Path,
    process::{Command, Stdio},
};

type Hwnd = *mut c_void;
type Bool = i32;
type EnumWindowsProc = Option<unsafe extern "system" fn(Hwnd, isize) -> Bool>;

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(callback: EnumWindowsProc, lparam: isize) -> Bool;
    fn IsWindowVisible(hwnd: Hwnd) -> Bool;
    fn GetWindowTextLengthW(hwnd: Hwnd) -> i32;
    fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, max_count: i32) -> i32;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DevHostPreparation {
    pub skip_vscode_semantic_restore: bool,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

pub fn prepare(snapshot: &Value, dry_run: bool) -> DevHostPreparation {
    let mut report = DevHostPreparation::default();
    let Some(editor) = snapshot
        .pointer("/editors/vscode")
        .filter(|value| !value.is_null())
    else {
        return report;
    };

    let saved_mode = editor.get("extensionMode").and_then(Value::as_str);
    let saved_path = editor.get("extensionPath").and_then(Value::as_str);
    let legacy_devhost = saved_desktop_mentions_devhost(snapshot);
    let devhost_visible = extension_development_host_visible();

    if saved_mode == Some("development") {
        let Some(extension_path) = saved_path.filter(|value| !value.trim().is_empty()) else {
            report.skip_vscode_semantic_restore = !devhost_visible;
            report.warnings.push(
                "VS Code restore: the saved Extension Development Host does not contain its development extension path; start the development host manually or re-save the capsule with the updated extension"
                    .to_owned(),
            );
            return report;
        };

        if devhost_visible {
            return report;
        }
        if !Path::new(extension_path).is_dir() {
            report.skip_vscode_semantic_restore = true;
            report.failures.push(format!(
                "VS Code restore: saved extension development path no longer exists: {extension_path}"
            ));
            return report;
        }

        let executable = saved_code_executable(snapshot).unwrap_or_else(|| "code".to_owned());
        if dry_run {
            report.warnings.push(format!(
                "VS Code restore: would start an Extension Development Host from '{extension_path}' using '{executable}'"
            ));
            return report;
        }

        match launch_devhost(&executable, extension_path) {
            Ok(()) => report.warnings.push(format!(
                "VS Code restore: started the saved Extension Development Host from '{extension_path}'"
            )),
            Err(error) => {
                report.skip_vscode_semantic_restore = true;
                report.failures.push(format!(
                    "VS Code restore: could not start the saved Extension Development Host: {error}"
                ));
            }
        }
        return report;
    }

    if saved_mode.is_none() && legacy_devhost && !devhost_visible {
        report.skip_vscode_semantic_restore = true;
        report.warnings.push(
            "VS Code restore: this capsule was saved from an Extension Development Host before Context Capsule captured its development extension path; its editor tabs cannot be auto-restored from this legacy capsule. Start the development host manually for this restore, then re-save the capsule once with the updated extension."
                .to_owned(),
        );
    }

    report
}

pub fn suppress_vscode_semantic(snapshot: &mut Value) {
    if let Some(vscode) = snapshot.pointer_mut("/editors/vscode") {
        *vscode = Value::Null;
    }
    if let Some(sessions) = snapshot
        .pointer_mut("/terminals/sessions")
        .and_then(Value::as_array_mut)
    {
        sessions.retain(|session| {
            session.get("host").and_then(Value::as_str) != Some("visual-studio-code")
        });
    }
}

fn launch_devhost(executable: &str, extension_path: &str) -> Result<(), String> {
    Command::new(executable)
        .arg("--new-window")
        .arg(format!("--extensionDevelopmentPath={extension_path}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to launch '{executable}': {error}"))
}

fn saved_code_executable(snapshot: &Value) -> Option<String> {
    let applications = snapshot.pointer("/desktop/applications")?.as_array()?;

    applications
        .iter()
        .filter(|application| application_mentions_devhost(application))
        .find_map(application_executable)
        .or_else(|| {
            applications.iter().find_map(|application| {
                let executable = application_executable(application)?;
                executable
                    .rsplit(['\\', '/'])
                    .next()
                    .is_some_and(|name| name.eq_ignore_ascii_case("code.exe"))
                    .then_some(executable)
            })
        })
}

fn application_executable(application: &Value) -> Option<String> {
    application
        .get("executable_path")
        .and_then(Value::as_str)
        .or_else(|| {
            application
                .get("launch")
                .filter(|launch| {
                    launch.get("strategy").and_then(Value::as_str) == Some("executable")
                })
                .and_then(|launch| launch.get("target"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

fn saved_desktop_mentions_devhost(snapshot: &Value) -> bool {
    snapshot
        .pointer("/desktop/applications")
        .and_then(Value::as_array)
        .is_some_and(|applications| applications.iter().any(application_mentions_devhost))
}

fn application_mentions_devhost(application: &Value) -> bool {
    application
        .get("windows")
        .and_then(Value::as_array)
        .is_some_and(|windows| {
            windows.iter().any(|window| {
                window
                    .get("title")
                    .and_then(Value::as_str)
                    .is_some_and(is_devhost_title)
            })
        })
}

fn is_devhost_title(title: &str) -> bool {
    title
        .to_ascii_lowercase()
        .contains("extension development host")
}

fn extension_development_host_visible() -> bool {
    let mut found = false;
    let data = (&mut found as *mut bool) as isize;
    unsafe {
        EnumWindows(Some(enum_window), data);
    }
    found
}

unsafe extern "system" fn enum_window(hwnd: Hwnd, data: isize) -> Bool {
    if unsafe { IsWindowVisible(hwnd) } == 0 {
        return 1;
    }
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return 1;
    }
    let mut buffer = vec![0_u16; length as usize + 1];
    let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied > 0 {
        let title = String::from_utf16_lossy(&buffer[..copied as usize]);
        if is_devhost_title(&title) {
            let found = unsafe { &mut *(data as *mut bool) };
            *found = true;
            return 0;
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_code_executable_from_saved_devhost_window() {
        let snapshot = json!({
            "desktop": {
                "applications": [{
                    "executable_path": "C:\\Program Files\\Microsoft VS Code\\Code.exe",
                    "windows": [{ "title": "project - Visual Studio Code [Extension Development Host]" }]
                }]
            }
        });
        assert_eq!(
            saved_code_executable(&snapshot).as_deref(),
            Some("C:\\Program Files\\Microsoft VS Code\\Code.exe")
        );
    }

    #[test]
    fn suppression_removes_editor_and_integrated_terminal_request() {
        let mut snapshot = json!({
            "editors": { "vscode": { "schemaVersion": 1 } },
            "terminals": {
                "sessions": [
                    { "host": "visual-studio-code" },
                    { "host": "windows-terminal" }
                ]
            }
        });
        suppress_vscode_semantic(&mut snapshot);
        assert!(snapshot.pointer("/editors/vscode").unwrap().is_null());
        assert_eq!(
            snapshot
                .pointer("/terminals/sessions")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
