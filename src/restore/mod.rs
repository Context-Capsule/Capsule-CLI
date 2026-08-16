mod model;

#[cfg(windows)]
#[path = "windows_core.rs"]
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

    let desktop = match SavedDesktop::from_capsule(snapshot) {
        Ok(Some(desktop)) => desktop,
        Ok(None) => {
            report
                .warnings
                .push("capsule has no restorable desktop snapshot".to_owned());
            return report;
        }
        Err(error) => {
            report.failures.push(error);
            return report;
        }
    };

    #[cfg(windows)]
    {
        report.desktop = windows::restore_desktop(&desktop, options.dry_run);
    }

    #[cfg(not(windows))]
    {
        let _ = (desktop, options);
        report
            .warnings
            .push("desktop restore is currently implemented for Windows only".to_owned());
    }

    report
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
    fn malformed_available_desktop_is_reported() {
        let report = restore_snapshot(
            &json!({ "desktop": { "status": "available", "applications": "wrong" } }),
            RestoreOptions { dry_run: true },
        );
        assert!(!report.success());
        assert_eq!(report.failures.len(), 1);
    }
}
