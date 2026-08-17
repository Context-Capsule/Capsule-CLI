mod model;
mod semantic;

#[cfg(windows)]
mod activation;
#[cfg(windows)]
mod dpi;
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
    let mut full_desktop = None;
    let defer_windows_terminal = should_defer_windows_terminal(snapshot);

    match SavedDesktop::from_capsule(snapshot) {
        Ok(Some(desktop)) => {
            let prerequisite = if defer_windows_terminal {
                desktop_without_windows_terminal(&desktop)
            } else {
                desktop.clone()
            };

            #[cfg(windows)]
            {
                let dpi_guard = dpi::DpiAwarenessGuard::per_monitor_v2();
                report.desktop = windows::restore_desktop(&prerequisite, options.dry_run);
                report.desktop.applications_total = desktop.applications.len();
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
            full_desktop = Some(desktop);
        }
        Ok(None) => report
            .warnings
            .push("capsule has no restorable desktop snapshot".to_owned()),
        Err(error) => report
            .failures
            .push(format!("desktop restore metadata: {error}")),
    }

    // A saved Extension Development Host is special: the Context Capsule extension
    // only exists inside that development host, so a generic Code.exe window cannot
    // consume the restore request. New capsules persist the extension development path
    // and we recreate the correct host before sending semantic work to it.
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

    // Semantic adapters run after the prerequisite desktop pass. Each adapter is
    // failure-isolated so one browser/editor/container problem cannot stop the rest.
    let semantic = semantic::restore(&semantic_snapshot, options.dry_run);
    report.warnings.extend(semantic.warnings);
    report.failures.extend(semantic.failures);

    // Explorer folder locations are captured separately from generic desktop geometry.
    // This avoids confusing the always-running Windows shell process with a folder
    // window that actually needs to be reopened.
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

    // Some desktop applications (notably packaged messaging apps) leave a background
    // process alive after their last visible window closes. A process-only check would
    // incorrectly consider those apps restored. Reactivate only saved apps that still
    // have a matching process but no visible top-level window, then let the final pass
    // place the resulting window.
    #[cfg(windows)]
    if !options.dry_run {
        if let Some(desktop) = full_desktop.as_ref() {
            let activation =
                activation::reactivate_background_only_apps(desktop, defer_windows_terminal);
            report.warnings.extend(activation.warnings);
            report.failures.extend(activation.failures);
        }
    }

    // Semantic adapters and reactivation can materialize the windows that should
    // actually be placed. Re-run the convergent desktop pass so geometry targets the
    // final browser/editor/terminal/Explorer/app windows rather than bootstrap hosts.
    #[cfg(windows)]
    if !options.dry_run {
        if let Some(desktop) = full_desktop.as_ref() {
            let initial_launched = report.desktop.applications_launched;
            let initial_already_running = report.desktop.applications_already_running;
            let initial_planned = report.desktop.applications_planned_to_launch;

            let dpi_guard = dpi::DpiAwarenessGuard::per_monitor_v2();
            let mut final_desktop = windows::restore_desktop(desktop, false);
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
                    && session.get("restart").is_some_and(|restart| !restart.is_null())
            })
        })
}

fn desktop_without_windows_terminal(desktop: &SavedDesktop) -> SavedDesktop {
    let mut filtered = desktop.clone();
    filtered
        .applications
        .retain(|application| !is_windows_terminal_application(application));
    filtered
}

fn is_windows_terminal_application(application: &SavedApplication) -> bool {
    if application
        .app_user_model_id
        .as_deref()
        .is_some_and(|value| value.to_ascii_lowercase().contains("windowsterminal"))
    {
        return true;
    }

    let executable = application
        .executable_path
        .as_deref()
        .or_else(|| application.launch.as_ref().map(|launch| launch.target.as_str()));
    if executable.is_some_and(|value| {
        value
            .rsplit(['\\', '/'])
            .next()
            .is_some_and(|name| {
                name.eq_ignore_ascii_case("windowsterminal.exe")
                    || name.eq_ignore_ascii_case("wt.exe")
            })
    }) {
        return true;
    }

    application.name.eq_ignore_ascii_case("Windows Terminal")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
