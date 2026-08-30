mod model;
mod semantic;

#[cfg(windows)]
mod activation;
#[cfg(windows)]
mod browser_bootstrap;
#[cfg(windows)]
mod custom_snap;
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
    let git = crate::git_context::restore_from_snapshot(snapshot, options.dry_run);
    report.warnings.extend(git.warnings);
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
                && desktop.applications.iter().any(is_zen_semantic_application);

            #[cfg(windows)]
            if zen_semantic_owner {
                zen_bootstrap = browser_bootstrap::ensure_zen_started(&desktop, options.dry_run);
                report.warnings.extend(zen_bootstrap.warnings.clone());
                report.failures.extend(zen_bootstrap.failures.clone());
            }

            // Semantic adapters own host/window creation first: keep Windows
            // Terminal, Zen and the VS Code Dev Host out of the prerequisite
            // desktop pass so generic launching cannot race their richer restore
            // logic or create an extra default terminal tab.
            let prerequisite = desktop_without_semantic_owned_hosts(
                &desktop,
                defer_windows_terminal,
                saved_devhost,
                zen_semantic_owner,
            );

            // After semantic restore has recreated browser/editor/terminal
            // topology, the final Win32 pass owns physical desktop convergence.
            // Windows Terminal must be included here: semantic terminal restore
            // creates the right tabs and CWDs, while this pass restores the saved
            // monitor, rectangle, maximized state and genuine Windows snap state.
            // This mirrors the original staged-restore contract: defer Terminal
            // startup, never defer its final placement.
            let mut final_target = desktop_without_semantic_owned_hosts(
                &desktop,
                false,
                saved_devhost && !has_vscode_semantic,
                false,
            );
            if zen_semantic_owner {
                disable_zen_title_identity_for_physical_matching(&mut final_target);
            }

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
        if zen_bootstrap.skip_semantic_restore {
            suppress_firefox_semantic(&mut semantic_snapshot);
        }

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
    if options.dry_run && explorer.planned_to_navigate > 0 {
        report.warnings.push(format!(
            "Explorer restore: would navigate {} unambiguous changed folder window(s) back to the saved target",
            explorer.planned_to_navigate
        ));
    }
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
            // Never skip the final physical pass merely because the inventory
            // captured before foreground/focus work already looked correct. The
            // saved geometry and window state are deliberately replayed after all
            // semantic tab/terminal work has finished.
            let mut final_desktop = windows::restore_desktop_forced(desktop, false);
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

            // Generic placement deliberately restores custom snap geometry first.
            // Once every semantic-owned window exists and is on the right monitor,
            // rebuild supported custom two-window layouts as genuine Windows Snap
            // groups by snapping a native pair and replaying the saved divider drag.
            // `desktop` is the physical-matching copy: Zen titles are intentionally
            // blank so both this pass and custom-Snap attribution use the geometry
            // established by the semantic adapter rather than volatile active-tab
            // titles.
            let custom = custom_snap::restore(desktop);
            final_desktop.warnings.extend(custom.warnings);
            final_desktop.failures.extend(custom.failures);

// Native Snap reconstruction and foreground acquisition can alter
// stacking after the generic Windows pass has already reconciled it.
// Make Z-order the final authoritative operation, using a fresh live
// inventory and no geometry/state changes.
let final_order = windows::restore_order_and_foreground_only(desktop);
final_desktop.warnings.extend(final_order.warnings);
final_desktop.failures.extend(final_order.failures);


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

#[cfg(any(windows, test))]
fn suppress_firefox_semantic(snapshot: &mut Value) {
    if let Some(browsers) = snapshot.get_mut("browsers").and_then(Value::as_object_mut) {
        browsers.insert("firefox".to_owned(), Value::Null);
    }
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
        .filter_map(|saved_application| {
            if defer_windows_terminal && is_windows_terminal_application(saved_application) {
                return None;
            }
            if defer_zen_browser && is_zen_semantic_application(saved_application) {
                return None;
            }

            let had_devhost = is_vscode_devhost_application(saved_application);
            let mut application = saved_application.clone();
            if defer_vscode_devhost && had_devhost {
                application
                    .windows
                    .retain(|window| !is_vscode_devhost_title(&window.title));
                if application.windows.is_empty() && !application.discovered_as_background {
                    return None;
                }
            }
            Some(application)
        })
        .collect();
    filtered
}

/// The Firefox/Zen semantic adapter has already decided which tab topology
/// belongs to each browser window and has restored every saved browser window's
/// geometry before the final Win32 pass begins. Browser chrome titles are just
/// the active tab title; they are therefore volatile and must not be used to
/// re-identify semantic windows in the physical-only phase.
///
/// Clearing titles on this cloned physical target makes the existing generic
/// and custom-Snap matchers fall back to geometry for Zen only. The original
/// capsule is untouched, and non-Zen applications retain title identity.
fn disable_zen_title_identity_for_physical_matching(desktop: &mut SavedDesktop) {
    for application in &mut desktop.applications {
        if !is_zen_semantic_application(application) {
            continue;
        }
        for window in &mut application.windows {
            window.title.clear();
        }
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
        .or_else(|| {
            application
                .launch
                .as_ref()
                .map(|launch| launch.target.as_str())
        })
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

    fn physical_test_window(title: &str) -> SavedWindow {
        SavedWindow {
            title: title.to_owned(),
            bounds: SavedRect {
                left: 0,
                top: 0,
                right: 900,
                bottom: 700,
            },
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
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("Firefox semantic restore"))
        );
    }

    #[test]
    fn zen_semantic_snapshot_is_deferred_only_before_semantic_restore() {
        let desktop = SavedDesktop {
            status: "available".to_owned(),
            displays: Vec::new(),
            applications: vec![
                application("zen", Some(r"C:\Program Files\Zen Browser\zen.exe")),
                application(
                    "Mozilla Firefox",
                    Some(r"C:\Program Files\Mozilla Firefox\firefox.exe"),
                ),
                application("Spotify", Some(r"C:\Users\me\Spotify.exe")),
            ],
        };
        let prerequisite = desktop_without_semantic_owned_hosts(&desktop, false, false, true);
        assert_eq!(prerequisite.applications.len(), 2);
        assert!(
            prerequisite
                .applications
                .iter()
                .any(|application| application.name == "Mozilla Firefox")
        );
        assert!(
            prerequisite
                .applications
                .iter()
                .any(|application| application.name == "Spotify")
        );
        assert!(
            !prerequisite
                .applications
                .iter()
                .any(is_zen_semantic_application)
        );

        let final_placement = desktop_without_semantic_owned_hosts(&desktop, false, false, false);
        assert_eq!(final_placement.applications.len(), 3);
        assert!(
            final_placement
                .applications
                .iter()
                .any(is_zen_semantic_application)
        );
    }

    #[test]
    fn zen_titles_are_disabled_only_in_the_physical_matching_copy() {
        let mut zen = application("Zen Browser", Some(r"C:\Program Files\Zen Browser\zen.exe"));
        zen.windows = vec![
            physical_test_window("Many tabs — Zen Browser"),
            physical_test_window("ChatGPT — Zen Browser"),
        ];
        zen.windows[1].z_order = 1;

        let mut other = application("Notepad", Some(r"C:\Windows\System32\notepad.exe"));
        other.windows = vec![physical_test_window("notes.txt - Notepad")];

        let original_zen_titles = zen
            .windows
            .iter()
            .map(|window| window.title.clone())
            .collect::<Vec<_>>();
        let desktop = SavedDesktop {
            status: "available".to_owned(),
            displays: Vec::new(),
            applications: vec![zen, other],
        };
        let mut physical = desktop.clone();
        disable_zen_title_identity_for_physical_matching(&mut physical);

        let zen_physical = physical
            .applications
            .iter()
            .find(|application| is_zen_semantic_application(application))
            .unwrap();
        assert!(
            zen_physical
                .windows
                .iter()
                .all(|window| window.title.is_empty())
        );
        assert_eq!(
            physical.applications[1].windows[0].title,
            "notes.txt - Notepad"
        );
        assert_eq!(
            desktop.applications[0]
                .windows
                .iter()
                .map(|window| window.title.clone())
                .collect::<Vec<_>>(),
            original_zen_titles
        );
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
    fn windows_terminal_is_deferred_for_startup_but_included_for_final_placement() {
        let desktop = SavedDesktop {
            status: "available".to_owned(),
            displays: Vec::new(),
            applications: vec![
                application(
                    "Windows Terminal",
                    Some(r"C:\Program Files\WindowsApps\Microsoft.WindowsTerminal\WindowsTerminal.exe"),
                ),
                application("Notepad", Some(r"C:\Windows\System32\notepad.exe")),
            ],
        };

        let prerequisite = desktop_without_semantic_owned_hosts(&desktop, true, false, false);
        assert!(!prerequisite.applications.iter().any(is_windows_terminal_application));
        assert!(prerequisite.applications.iter().any(|app| app.name == "Notepad"));

        let final_placement = desktop_without_semantic_owned_hosts(&desktop, false, false, false);
        assert!(final_placement.applications.iter().any(is_windows_terminal_application));
        assert!(final_placement.applications.iter().any(|app| app.name == "Notepad"));
    }

    #[test]
    fn suppressing_firefox_semantic_only_removes_the_browser_payload() {
        let mut snapshot = json!({
            "browsers": { "firefox": { "windows": [1] } },
            "editors": { "vscode": { "tabs": [1] } }
        });
        suppress_firefox_semantic(&mut snapshot);
        assert!(snapshot.pointer("/browsers/firefox").unwrap().is_null());
        assert_eq!(snapshot.pointer("/editors/vscode/tabs/0"), Some(&json!(1)));
    }

    #[test]
    fn devhost_window_is_removed_without_dropping_normal_code_windows() {
        let mut code = application(
            "Visual Studio Code",
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
        );
        let base = SavedWindow {
            title: "ordinary - Visual Studio Code".to_owned(),
            bounds: SavedRect {
                left: 0,
                top: 0,
                right: 900,
                bottom: 700,
            },
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
        let desktop = SavedDesktop {
            status: "available".to_owned(),
            displays: vec![],
            applications: vec![code],
        };
        let filtered = desktop_without_semantic_owned_hosts(&desktop, false, true, false);
        assert_eq!(filtered.applications.len(), 1);
        assert_eq!(filtered.applications[0].windows.len(), 1);
        assert!(!is_vscode_devhost_title(
            &filtered.applications[0].windows[0].title
        ));
    }

    #[test]
    fn devhost_only_application_is_removed_from_pre_semantic_desktop() {
        let mut code = application(
            "Visual Studio Code",
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
        );
        code.windows = vec![SavedWindow {
            title: "extension - Visual Studio Code [Extension Development Host]".to_owned(),
            bounds: SavedRect {
                left: 100,
                top: 100,
                right: 1000,
                bottom: 800,
            },
            restore_bounds: None,
            normalized_bounds: None,
            state: "normal".to_owned(),
            display_device: "DISPLAY1".to_owned(),
            display_relation: "primary".to_owned(),
            display_scale_percent: 100,
            is_foreground: true,
            z_order: 0,
            virtual_desktop_id: None,
            is_on_current_virtual_desktop: Some(true),
            taskbar_candidate: true,
        }];
        let desktop = SavedDesktop {
            status: "available".to_owned(),
            displays: vec![],
            applications: vec![code],
        };
        assert!(
            desktop_without_semantic_owned_hosts(&desktop, false, true, false)
                .applications
                .is_empty()
        );
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
        let sessions = snapshot
            .pointer("/terminals/sessions")
            .unwrap()
            .as_array()
            .unwrap();
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
        packaged.app_user_model_id = Some("Microsoft.WindowsTerminal_8wekyb3d8bbwe!App".to_owned());
        assert!(is_windows_terminal_application(&packaged));
    }
}
