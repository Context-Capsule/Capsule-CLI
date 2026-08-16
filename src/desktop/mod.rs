mod classify;
mod model;

pub use model::{ApplicationInfo, DesktopSnapshot, IgnoredCandidate, WindowInfo};

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub fn discover() -> Result<DesktopSnapshot, String> {
    windows::discover()
}

#[cfg(not(windows))]
pub fn discover() -> Result<DesktopSnapshot, String> {
    Err("desktop discovery is currently supported on Windows only".to_owned())
}

#[cfg(test)]
mod tests {
    use super::model::{ApplicationClassification, Rect, WindowState};
    use super::*;

    #[test]
    fn virtual_desktops_are_grouped_from_application_windows() {
        let window = WindowInfo {
            title: "Editor".to_owned(),
            bounds: Rect {
                left: 0,
                top: 0,
                right: 100,
                bottom: 100,
            },
            restore_bounds: None,
            normalized_bounds: None,
            state: WindowState::Normal,
            display_device: "DISPLAY1".to_owned(),
            display_relation: "primary".to_owned(),
            display_scale_percent: 100,
            is_foreground: true,
            z_order: 0,
            virtual_desktop_id: Some("desktop-a".to_owned()),
            is_on_current_virtual_desktop: Some(true),
            taskbar_candidate: true,
        };

        let snapshot = DesktopSnapshot {
            displays: vec![],
            applications: vec![ApplicationInfo {
                primary_pid: 1,
                pids: vec![1],
                parent_pid: None,
                name: "Editor".to_owned(),
                executable_path: None,
                app_user_model_id: None,
                file_version: None,
                classification: ApplicationClassification::UserApplication,
                confidence: 100,
                classification_reason: "test".to_owned(),
                launch: None,
                windows: vec![window],
                discovered_as_background: false,
            }],
            ignored: vec![],
        };

        assert_eq!(
            snapshot.virtual_desktops(),
            vec![("desktop-a".to_owned(), Some(true), 1)]
        );
    }
}
