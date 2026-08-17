mod model;
mod semantic;

#[cfg(windows)]
mod activation;
#[cfg(windows)]
mod dpi;
#[cfg(windows)]
mod legacy_explorer;
#[cfg(windows)]
mod vscode_devhost;
#[cfg(windows)]
#[allow(dead_code)]
mod windows;

use serde_json::Value;

pub use model::{
    SavedApplication, SavedDesktop, SavedDisplay, SavedLaunchSpec, SavedNormalizedRect, SavedRect,
    SavedWindow, SnapSlot, TargetDisplay, WindowStateSpec, choose_display, rect_close, snap_rect,
    target_rect, title_match_score,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestoreOptions {
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopRestoreReport {
    pub applications_total: usize,
    pub applications_already_running: usize,
    pub applications_planned_to_launch: usize,
    pub applications_launched: usize,
    pub applications_unlaunchable: usize,
    pub applications_failed: usize,
    pub windows_total: usize,
    pub windows_already_placed: usize,
    pub windows_planned_to_move: usize,
    pub windows_moved: usize,
    pub windows_missing: usize,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

impl DesktopRestoreReport {
    pub fn success(&self) -> bool {
        self.failures.is_empty() && self.applications_failed == 0
    }

    pub fn changed(&self) -> bool {
        self.applications_launched > 0 || self.windows_moved > 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub desktop: DesktopRestoreReport,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

impl RestoreReport {
    pub fn success(&self) -> bool {
        self.failures.is_empty() && self.desktop.success()
    }
}

pub fn restore_snapshot(snapshot: &Value, options: RestoreOptions) -> RestoreReport {
    let mut report = RestoreReport::default();
    let mut generic_desktop = None;
    let mut full_application_count = 0usize;
    let defer_windows_terminal = should_defer_windows_terminal(snapshot);
    let defer_vscode_devhost = should_defer_vscode_devhost(snapshot);
    let defer_firefox_browser = has_firefox_browser_snapshot(snapshot);

    match SavedDesktop::from_capsule(snapshot) {
        Ok(Some(desktop)) => {
            full_application_count = desktop.applications.len();
            let prerequisite = desktop_without_semantic_owned_hosts(
                &desktop,
                defer_windows_terminal,
                defer_vscode_devhost,
                defer_firefox_browser,
            );

            #[cfg(windows)]
            {
                let dpi_guard = dpi::DpiAwarenessGuard::per_monitor_v2();
                report.desktop = windows::restore_desktop(&prerequisite, options.dry_run);
                report.desktop.applications_total = full_application_count;
                if dpi_guard.is_none() {
                    report.desktop.warnings.push(
                        "could not switch the restore thread to Per-Monitor-V2 DPI awareness; placement may be less accurate on mixed-DPI displays"
                            .to_owned(),
                    );
                }
            }

            #[cfg(not(windows))]
            {
                let _ = &prerequisite;
                report
                    .warnings
                    .push("desktop restore is currently implemented for Windows only".to_owned());
            }

            if defer_windows_terminal {
                report.warnings.push(
                    "Windows Terminal startup is delegated to the semantic terminal adapter so restore does not create an extra default tab before recreating the saved sessions"
                        .to_owned(),
                );
            }
            if defer_vscode_devhost {
                report.warnings.push(
                    "VS Code Extension Development Host startup is delegated to its semantic adapter so restore does not create an extra normal Code window first"
                        .to_owned(),
                );
            }
            if defer_firefox_browser {
                report.warnings.push(
                    "Firefox/Zen browser window creation and placement are delegated to the semantic browser adapter so Zen Window Sync cannot clone or mutate the current Space during generic desktop restore"
                        .to_owned(),
                );
            }
            generic_desktop = Some(prerequisite);
        }
        Ok(None) => report
            .warnings
            .push("capsule has no restorable desktop snapshot".to_owned()),
        Err(error) => report
            .failures
            .push(format!("desktop restore metadata: {error}")),
    }

    let mut semantic_snapshot = snapshot.clone();
    #[cfg(windows)]
    {
        let preparation = vscode_devhost::prepare(snapshot, options.dry_run);
        if preparation.skip_vscode_semantic_restore {
            vscode_devhost::suppress_vscode_semantic(&mut semantic_snapshot);
        }
        report.warnings.extend(preparation.warnings);
        report.failures.extend(preparation.failures);
    }

    let orphaned_vscode_terminals = suppress_orphan_vscode_semantic(&mut semantic_snapshot);
    if orphaned_vscode_terminals > 0 {
        report.warnings.push(format!(
            "VS Code semantic restore skipped {orphaned_vscode_terminals} integrated terminal session(s) because this capsule contains no VS Code editor snapshot to identify a target extension host; legacy terminal metadata alone cannot be routed safely"
        ));
    }
    if !has_vscode_editor_snapshot(snapshot) && saved_desktop_mentions_devhost(snapshot) {
        report.warnings.push(
            "VS Code restore: the capsule contains an Extension Development Host window but no semantic editor snapshot. Its open editor tabs were not captured in this capsule, so they cannot be reconstructed exactly; re-save once with the updated VS Code extension to preserve them."
                .to_owned(),
        );
    }

    let semantic = semantic::restore(&semantic_snapshot, options.dry_run);
    report.warnings.extend(semantic.warnings);
    report.failures.extend(semantic.failures);

    let explorer = crate::explorer::restore_from_capsule(snapshot, options.dry_run);
    if options.dry_run && explorer.planned_to_open > 0 {
        report.warnings.push(format!(
            "Explorer restore: would open {} missing folder window(s)",
            explorer.planned_to_open
        ));
    } else if explorer.opened > 0 {
        report.warnings.push(format!(
            "Explorer restore: opened {} missing folder window(s)",
            explorer.opened
        ));
    }
    report.warnings.extend(explorer.warnings);
    report.failures.extend(explorer.failures);

    #[cfg(windows)]
    {
        let legacy_explorer = legacy_explorer::restore(snapshot, options.dry_run);
        if options.dry_run && legacy_explorer.planned > 0 {
            report.warnings.push(format!(
                "Legacy Explorer restore: would open {} safe Home/Quick access window(s)",
                legacy_explorer.planned
            ));
        } else if legacy_explorer.opened > 0 {
            report.warnings.push(format!(
                "Legacy Explorer restore: opened {} safe Home/Quick access window(s)",
                legacy_explorer.opened
            ));
        }
        report.warnings.extend(legacy_explorer.warnings);
        report.failures.extend(legacy_explorer.failures);
    }

    #[cfg(windows)]
    if !options.dry_run {
        if let Some(desktop) = generic_desktop.as_ref() {
            let activation =
                activation::reactivate_background_only_apps(desktop, defer_windows_terminal);
            report.warnings.extend(activation.warnings);
            report.failures.extend(activation.failures);
        }
    }

    #[cfg(windows)]
    if !options.dry_run {
        if let Some(desktop) = generic_desktop.as_ref() {
            let initial_launched = report.desktop.applications_launched;
            let initial_already_running = report.desktop.applications_already_running;
            let initial_planned = report.desktop.applications_planned_to_launch;

            let dpi_guard = dpi::DpiAwarenessGuard::per_monitor_v2();
            let mut final_desktop = windows::restore_desktop(desktop, false);
            final_desktop.applications_total = full_application_count;
            final_desktop.applications_launched += initial_launched;
            final_desktop.applications_already_running = initial_already_running;
            final_desktop.applications_planned_to_launch = initial_planned;
            if dpi_guard.is_none() {
                final_desktop.warnings.push(
                    "could not switch the final placement pass to Per-Monitor-V2 DPI awareness; placement may be less accurate on mixed-DPI displays"
                        .to_owned(),
                );
            }
            report.desktop = final_desktop;
        }
    }

    report
}

fn should_defer_windows_terminal(snapshot: &Value) -> bool {
    snapshot
        .pointer("/terminals/sessions")
        .and_then(Value::as_array)
        .is_some_and(|sessions| {
            sessions.iter().any(|session| {
                session.get("host").and_then(Value::as_str) == Some("windows-terminal")
                    && session
                        .get("restart")
                        .is_some_and(|restart| !restart.is_null())
            })
        })
}

fn has_firefox_browser_snapshot(snapshot: &Value) -> bool {
    snapshot
        .pointer("/browsers/firefox")
        .is_some_and(|value| !value.is_null())
}

fn has_vscode_editor_snapshot(snapshot: &Value) -> bool {
    snapshot
        .pointer("/editors/vscode")
        .is_some_and(|value| !value.is_null())
}

fn suppress_orphan_vscode_semantic(snapshot: &mut Value) -> usize {
    if has_vscode_editor_snapshot(snapshot) {
        return 0;
    }

    let Some(sessions) = snapshot
        .pointer_mut("/terminals/sessions")
        .and_then(Value::as_array_mut)
    else {
        return 0;
    };
    let before = sessions.len();
    sessions.retain(|session| {
        session.get("host").and_then(Value::as_str) != Some("visual-studio-code")
    });
    before.saturating_sub(sessions.len())
}

fn should_defer_vscode_devhost(snapshot: &Value) -> bool {
    let editor = snapshot
        .pointer("/editors/vscode")
        .filter(|value| !value.is_null());
    if editor
        .and_then(|value| value.get("extensionMode"))
        .and_then(Value::as_str)
        == Some("development")
    {
        return true;
    }

    // A legacy window title is only actionable when a semantic editor snapshot
    // exists. Deferring an entire Code application without editor data can leave
    // the restore with no component responsible for starting or targeting it.
    editor.is_some() && saved_desktop_mentions_devhost(snapshot)
}

fn saved_desktop_mentions_devhost(snapshot: &Value) -> bool {
    snapshot
        .pointer("/desktop/applications")
        .and_then(Value::as_array)
        .is_some_and(|applications| {
            applications.iter().any(|application| {
                application
                    .get("windows")
                    .and_then(Value::as_array)
                    .is_some_and(|windows| {
                        windows.iter().any(|window| {
                            window
                                .get("title")
                                .and_then(Value::as_str)
                                .is_some_and(is_vscode_devhost_title)
                        })
                    })
            })
        })
}

fn desktop_without_semantic_owned_hosts(
    desktop: &SavedDesktop,
    defer_windows_terminal: bool,
    defer_vscode_devhost: bool,
    defer_firefox_browser: bool,
) -> SavedDesktop {
    let mut filtered = desktop.clone();
    filtered.applications.retain(|application| {
        !(defer_windows_terminal && is_windows_terminal_application(application))
            && !(defer_vscode_devhost && is_vscode_devhost_application(application))
            && !(defer_firefox_browser && is_firefox_semantic_application(application))
    });
    filtered
}

fn is_vscode_devhost_title(title: &str) -> bool {
    title
        .to_ascii_lowercase()
        .contains("extension development host")
}

fn is_vscode_devhost_application(application: &SavedApplication) -> bool {
    application
        .windows
        .iter()
        .any(|window| is_vscode_devhost_title(&window.title))
}

fn executable_basename(application: &SavedApplication) -> Option<&str> {
    application
        .executable_path
        .as_deref()
        .or_else(|| application.launch.as_ref().map(|launch| launch.target.as_str()))
        .and_then(|value| value.rsplit(['\\', '/']).next())
}

fn is_firefox_semantic_application(application: &SavedApplication) -> bool {
    if executable_basename(application).is_some_and(|name| {
        name.eq_ignore_ascii_case("zen.exe")
            || name.eq_ignore_ascii_case("zen")
            || name.eq_ignore_ascii_case("firefox.exe")
            || name.eq_ignore_ascii_case("firefox")
    }) {
        return true;
    }

    application.name.eq_ignore_ascii_case("zen")
        || application.name.eq_ignore_ascii_case("Zen Browser")
        || application.name.eq_ignore_ascii_case("firefox")
        || application.name.eq_ignore_ascii_case("Mozilla Firefox")
}

fn is_windows_terminal_application(application: &SavedApplication) -> bool {
    if application
        .app_user_model_id
        .as_deref()
        .is_some_and(|value| value.to_ascii_lowercase().contains("windowsterminal"))
    {
        return true;
    }

    if executable_basename(application).is_some_and(|name| {
        name.eq_ignore_ascii_case("windowsterminal.exe") || name.eq_ignore_ascii_case("wt.exe")
    }) {
        return true;
    }

    application.name.eq_ignore_ascii_case("Windows Terminal")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn application(name: &str, executable_path: Option<&str>) -> SavedApplication {
        SavedApplication {
            name: name.to_owned(),
            executable_path: executable_path.map(str::to_owned),
            app_user_model_id: None,
            file_version: None,
            classification: "user-application".to_owned(),
            launch: None,
            windows: Vec::new(),
            discovered_as_background: false,
        }
    }

    #[test]
    fn missing_desktop_is_a_graceful_noop() {
        let report = restore_snapshot(&json!({}), RestoreOptions { dry_run: true });
        assert!(report.success());
        assert_eq!(report.desktop.applications_total, 0);
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn unavailable_desktop_is_a_graceful_noop() {
        let report = restore_snapshot(
            &json!({ "desktop": { "status": "unavailable", "message": "not captured" } }),
            RestoreOptions { dry_run: true },
        );
        assert!(report.success());
        assert_eq!(report.desktop.applications_total, 0);
    }

    #[test]
    fn malformed_available_desktop_is_reported_without_panicking() {
        let report = restore_snapshot(
            &json!({ "desktop": { "status": "available", "applications": "wrong" } }),
            RestoreOptions { dry_run: true },
        );
        assert!(!report.success());
        assert_eq!(report.failures.len(), 1);
    }

    #[test]
    fn semantic_dry_run_does_not_require_a_desktop_snapshot() {
        let report = restore_snapshot(
            &json!({
                "browsers": { "firefox": { "schema_version": 1, "browser": "firefox" } }
            }),
            RestoreOptions { dry_run: true },
        );
        assert!(report.success());
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("Firefox semantic restore")));
    }

    #[test]
    fn semantic_browser_snapshot_owns_zen_and_firefox_desktop_apps() {
        let desktop = SavedDesktop {
            displays: Vec::new(),
            applications: vec![
                application("zen", Some(r"C:\Program Files\Zen Browser\zen.exe")),
                application("Mozilla Firefox", Some(r"C:\Program Files\Mozilla Firefox\firefox.exe")),
                application("Spotify", Some(r"C:\Users\me\Spotify.exe")),
            ],
        };
        let filtered = desktop_without_semantic_owned_hosts(&desktop, false, false, true);
        assert_eq!(filtered.applications.len(), 1);
        assert_eq!(filtered.applications[0].name, "Spotify");

        let unfiltered = desktop_without_semantic_owned_hosts(&desktop, false, false, false);
        assert_eq!(unfiltered.applications.len(), 3);
    }

    #[test]
    fn windows_terminal_is_deferred_only_when_a_safe_terminal_plan_exists() {
        assert!(should_defer_windows_terminal(&json!({
            "terminals": {
                "sessions": [{
                    "host": "windows-terminal",
                    "restart": { "executable": "wt.exe", "args": [] }
                }]
            }
        })));
        assert!(!should_defer_windows_terminal(&json!({
            "terminals": {
                "sessions": [{ "host": "windows-terminal", "restart": null }]
            }
        })));
    }

    #[test]
    fn vscode_devhost_is_deferred_only_when_semantic_editor_state_can_target_it() {
        assert!(should_defer_vscode_devhost(&json!({
            "editors": { "vscode": { "extensionMode": "development" } }
        })));
        assert!(should_defer_vscode_devhost(&json!({
            "editors": { "vscode": { "schemaVersion": 1 } },
            "desktop": { "applications": [{
                "windows": [{ "title": "project - Visual Studio Code [Extension Development Host]" }]
            }] }
        })));
        assert!(!should_defer_vscode_devhost(&json!({
            "editors": { "vscode": null },
            "desktop": { "applications": [{
                "windows": [{ "title": "project - Visual Studio Code [Extension Development Host]" }]
            }] }
        })));
    }

    #[test]
    fn orphan_vscode_terminals_are_removed_without_touching_external_sessions() {
        let mut snapshot = json!({
            "editors": { "vscode": null },
            "terminals": {
                "sessions": [
                    { "host": "visual-studio-code" },
                    { "host": "visual-studio-code" },
                    { "host": "windows-terminal" }
                ]
            }
        });
        assert_eq!(suppress_orphan_vscode_semantic(&mut snapshot), 2);
        let sessions = snapshot.pointer("/terminals/sessions").unwrap().as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["host"], "windows-terminal");
    }

    #[test]
    fn vscode_terminals_remain_when_editor_snapshot_exists() {
        let mut snapshot = json!({
            "editors": { "vscode": { "schemaVersion": 1 } },
            "terminals": { "sessions": [{ "host": "visual-studio-code" }] }
        });
        assert_eq!(suppress_orphan_vscode_semantic(&mut snapshot), 0);
        assert_eq!(snapshot.pointer("/terminals/sessions").unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn windows_terminal_identity_detection_handles_paths_and_aumids() {
        let base = SavedApplication {
            name: "Terminal".to_owned(),
            executable_path: Some(
                r"C:\Program Files\WindowsApps\Microsoft.WindowsTerminal\WindowsTerminal.exe"
                    .to_owned(),
            ),
            app_user_model_id: None,
            file_version: None,
            classification: "user-application".to_owned(),
            launch: None,
            windows: Vec::new(),
            discovered_as_background: false,
        };
        assert!(is_windows_terminal_application(&base));

        let mut packaged = base.clone();
        packaged.executable_path = None;
        packaged.app_user_model_id = Some(
            "Microsoft.WindowsTerminal_8wekyb3d8bbwe!App".to_owned(),
        );
        assert!(is_windows_terminal_application(&packaged));
    }
}
