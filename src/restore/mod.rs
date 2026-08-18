mod model;
mod semantic;

#[cfg(windows)]
mod activation;
#[cfg(windows)]
mod browser_bootstrap;
#[cfg(windows)]
mod dpi;
#[cfg(windows)]
mod legacy_explorer;
#[cfg(windows)]
mod vscode_devhost;
#[cfg(windows)]
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
    let mut activation_desktop = None;
    let mut final_placement_desktop = None;
    let mut full_application_count = 0usize;
    #[cfg(windows)]
    let mut zen_bootstrap = browser_bootstrap::ZenBootstrapReport::default();
    let defer_windows_terminal = should_defer_windows_terminal(snapshot);
    let saved_devhost = saved_desktop_mentions_devhost(snapshot);
    let has_vscode_semantic = has_vscode_editor_snapshot(snapshot);
    let has_browser_semantic = has_firefox_browser_snapshot(snapshot);

    match SavedDesktop::from_capsule(snapshot) {
        Ok(Some(desktop)) => {
            full_application_count = desktop.applications.len();
            let zen_semantic_owner = has_browser_semantic
                && desktop
                    .applications
                    .iter()
                    .any(is_zen_semantic_application);

            #[cfg(windows)]
            if zen_semantic_owner {
                zen_bootstrap = browser_bootstrap::ensure_zen_started(&desktop, options.dry_run);
                report.warnings.extend(zen_bootstrap.warnings.clone());
                report.failures.extend(zen_bootstrap.failures.clone());
            }

            // Never let generic launch/placement touch an Extension Development
            // Host before the semantic adapter has identified the correct host.
            let prerequisite = desktop_without_semantic_owned_hosts(
                &desktop,
                defer_windows_terminal,
                saved_devhost,
                zen_semantic_owner,
            );

            // If semantic editor data exists, add the Dev Host back only for the
            // final placement pass, after its semantic restore has completed. If
            // editor data is missing, leave that window untouched rather than
            // resizing or launching a normal Code window as a substitute.
            let final_target = desktop_without_semantic_owned_hosts(
                &desktop,
                defer_windows_terminal,
                saved_devhost && !has_vscode_semantic,
                zen_semantic_owner,
            );

            #[cfg(windows)]
            {
                let dpi_guard = dpi::DpiAwarenessGuard::per_monitor_v2();
                report.desktop = windows::restore_desktop(&prerequisite, options.dry_run);
                report.desktop.applications_total = full_application_count;
                if zen_bootstrap.already_running {
                    report.desktop.applications_already_running += 1;
                }
                if zen_bootstrap.planned {
                    report.desktop.applications_planned_to_launch += 1;
                }
                if zen_bootstrap.launched {
                    report.desktop.applications_launched += 1;
                }
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

            activation_desktop = Some(prerequisite);
            final_placement_desktop = Some(final_target);
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
            "Skipped {orphaned_vscode_terminals} legacy VS Code terminal session(s): this capsule has no semantic VS Code host identity."
        ));
    }
    if !has_vscode_semantic && saved_devhost {
        report.warnings.push(
            "The capsule contains an Extension Development Host but no semantic VS Code snapshot. Context Capsule left that Dev Host's tabs, terminals, and geometry untouched because it cannot route them safely without host identity data."
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
        }
        report.warnings.extend(legacy_explorer.warnings);
        report.failures.extend(legacy_explorer.failures);
    }

    #[cfg(windows)]
    if !options.dry_run {
        if let Some(desktop) = activation_desktop.as_ref() {
            let activation =
                activation::reactivate_background_only_apps(desktop, defer_windows_terminal);
            report.warnings.extend(activation.warnings);
            report.failures.extend(activation.failures);
        }
    }

    #[cfg(windows)]
    if !options.dry_run {
        if let Some(desktop) = final_placement_desktop.as_ref() {
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
    defer_zen_browser: bool,
) -> SavedDesktop {
    let mut filtered = desktop.clone();
    filtered.applications = desktop
        .applications
        .iter()
        .filter_map(|application| {
            if defer_windows_terminal && is_windows_terminal_application(application) {
                return None;
            }
            if defer_zen_browser && is_zen_semantic_application(application) {
                return None;
            }

            let mut application = application.clone();
            if defer_vscode_devhost {
                application
                    .windows
                    .retain(|window| !is_vscode_devhost_title(&window.title));
                if application.windows.is_empty()
                    && !application.discovered_as_background
                    && is_vscode_devhost_application(application.as_ref_or_self())
                {
                    return None;
                }
            }
            Some(application)
        })
        .collect();
    filtered
}

trait ApplicationSelfRef {
    fn as_ref_or_self(&self) -> &SavedApplication;
}

impl ApplicationSelfRef for SavedApplication {
    fn as_ref_or_self(&self) -> &SavedApplication {
        self
    }
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

fn is_zen_semantic_application(application: &SavedApplication) -> bool {
    if executable_basename(application).is_some_and(|name| {
        name.eq_ignore_ascii_case("zen.exe") || name.eq_ignore_ascii_case("zen")
    }) {
        return true;
    }

    application.name.eq_ignore_ascii_case("zen")
        || application.name.eq_ignore_ascii_case("Zen Browser")
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
    fn zen_semantic_snapshot_owns_only_zen_desktop_apps() {
        let desktop = SavedDesktop {
            status: "available".to_owned(),
            displays: Vec::new(),
            applications: vec![
                application("zen", Some(r"C:\Program Files\Zen Browser\zen.exe")),
                application("Mozilla Firefox", Some(r"C:\Program Files\Mozilla Firefox\firefox.exe")),
                application("Spotify", Some(r"C:\Users\me\Spotify.exe")),
            ],
        };
        let filtered = desktop_without_semantic_owned_hosts(&desktop, false, false, true);
        assert_eq!(filtered.applications.len(), 2);
        assert!(filtered.applications.iter().any(|application| application.name == "Mozilla Firefox"));
        assert!(filtered.applications.iter().any(|application| application.name == "Spotify"));
    }

    #[test]
    fn windows_terminal_is_deferred_only_when_a_safe_terminal_plan_exists() {
        assert!(should_defer_windows_terminal(&json!({
            "terminals": { "sessions": [{ "host": "windows-terminal", "restart": { "executable": "wt.exe", "args": [] } }] }
        })));
        assert!(!should_defer_windows_terminal(&json!({
            "terminals": { "sessions": [{ "host": "windows-terminal", "restart": null }] }
        })));
    }

    #[test]
    fn devhost_window_is_removed_without_dropping_normal_code_windows() {
        let mut code = application("Visual Studio Code", Some(r"C:\Program Files\Microsoft VS Code\Code.exe"));
        let base = SavedWindow {
            title: "ordinary - Visual Studio Code".to_owned(),
            bounds: SavedRect { left: 0, top: 0, right: 900, bottom: 700 },
            restore_bounds: None,
            normalized_bounds: None,
            state: "normal".to_owned(),
            display_device: "DISPLAY1".to_owned(),
            display_relation: "primary".to_owned(),
            display_scale_percent: 100,
            is_foreground: false,
            z_order: 0,
            virtual_desktop_id: None,
            is_on_current_virtual_desktop: Some(true),
            taskbar_candidate: true,
        };
        let mut dev = base.clone();
        dev.title = "extension - Visual Studio Code [Extension Development Host]".to_owned();
        code.windows = vec![base, dev];
        let desktop = SavedDesktop { status: "available".to_owned(), displays: vec![], applications: vec![code] };
        let filtered = desktop_without_semantic_owned_hosts(&desktop, false, true, false);
        assert_eq!(filtered.applications.len(), 1);
        assert_eq!(filtered.applications[0].windows.len(), 1);
        assert!(!is_vscode_devhost_title(&filtered.applications[0].windows[0].title));
    }

    #[test]
    fn orphan_vscode_terminals_are_removed_without_touching_external_sessions() {
        let mut snapshot = json!({
            "editors": { "vscode": null },
            "terminals": { "sessions": [
                { "host": "visual-studio-code" },
                { "host": "visual-studio-code" },
                { "host": "windows-terminal" }
            ] }
        });
        assert_eq!(suppress_orphan_vscode_semantic(&mut snapshot), 2);
        let sessions = snapshot.pointer("/terminals/sessions").unwrap().as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["host"], "windows-terminal");
    }
}
