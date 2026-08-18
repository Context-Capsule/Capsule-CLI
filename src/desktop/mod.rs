mod classify;
mod model;

pub use model::{
    ApplicationInfo, DesktopSnapshot, DisplayInfo, IgnoredCandidate, Rect, WindowInfo,
};

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub fn discover() -> Result<DesktopSnapshot, String> {
    let mut snapshot = windows::discover()?;
    annotate_direct_application_owners(&mut snapshot);
    Ok(snapshot)
}

#[cfg(not(windows))]
pub fn discover() -> Result<DesktopSnapshot, String> {
    Err("desktop discovery is currently supported on Windows only".to_owned())
}

pub fn application_running_by_executable_name(
    executable_names: &[&str],
) -> Result<bool, String> {
    let snapshot = discover()?;
    Ok(snapshot.applications.iter().any(|application| {
        application
            .executable_path
            .as_deref()
            .and_then(|path| path.rsplit(['\\', '/']).next())
            .is_some_and(|name| {
                executable_names
                    .iter()
                    .any(|expected| name.eq_ignore_ascii_case(expected))
            })
    }))
}

fn annotate_direct_application_owners(snapshot: &mut DesktopSnapshot) {
    let applications = &snapshot.applications;

    for candidate in &mut snapshot.ignored {
        let Some(parent_pid) = candidate.parent_pid else {
            continue;
        };
        let Some(owner) = applications
            .iter()
            .find(|application| application.pids.contains(&parent_pid))
        else {
            continue;
        };

        candidate.reason = format!(
            "{}; child of captured application {}",
            candidate.reason, owner.name
        );
    }
}

#[cfg(test)]
mod tests {
    use super::model::{ApplicationClassification, WindowState};
    use super::*;

    fn application(pid: u32, name: &str, windows: Vec<WindowInfo>) -> ApplicationInfo {
        ApplicationInfo {
            primary_pid: pid,
            pids: vec![pid],
            parent_pid: None,
            name: name.to_owned(),
            executable_path: None,
            app_user_model_id: None,
            file_version: None,
            classification: ApplicationClassification::UserApplication,
            confidence: 100,
            classification_reason: "test".to_owned(),
            launch: None,
            windows,
            discovered_as_background: false,
        }
    }

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
            applications: vec![application(1, "Editor", vec![window])],
            ignored: vec![],
        };

        assert_eq!(
            snapshot.virtual_desktops(),
            vec![("desktop-a".to_owned(), Some(true), 1)]
        );
    }

    #[test]
    fn helper_is_annotated_when_parent_is_a_captured_application() {
        let mut snapshot = DesktopSnapshot {
            displays: vec![],
            applications: vec![application(42, "Docker Desktop", vec![])],
            ignored: vec![IgnoredCandidate {
                pid: 99,
                parent_pid: Some(42),
                executable: "OmApSvcBroker.exe".to_owned(),
                executable_path: None,
                window_title: Some("OmApSvcBroker".to_owned()),
                classification: ApplicationClassification::ApplicationHelper,
                confidence: 90,
                reason: "broker/helper/service-style process".to_owned(),
            }],
        };

        annotate_direct_application_owners(&mut snapshot);

        assert!(snapshot.ignored[0].reason.contains("Docker Desktop"));
    }
}
