use crate::explorer::{self, ExplorerStatus};
use serde_json::Value;
use std::{
    process::{Command, Stdio},
    thread,
    time::Duration,
};

const LAUNCH_SPACING: Duration = Duration::from_millis(120);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacyExplorerReport {
    pub planned: usize,
    pub opened: usize,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

pub fn restore(snapshot: &Value, dry_run: bool) -> LegacyExplorerReport {
    let mut report = LegacyExplorerReport::default();

    // New capsules contain exact Explorer targets and are handled by explorer.rs.
    // This fallback exists only for capsules captured before that schema slot existed.
    if snapshot.get("explorer").is_some() {
        return report;
    }

    let saved_home_windows = saved_legacy_home_window_count(snapshot);
    if saved_home_windows == 0 {
        return report;
    }

    let current = explorer::discover();
    let current_home_windows = if current.status == ExplorerStatus::Available {
        current
            .windows
            .iter()
            .filter(|window| window.title.as_deref().is_some_and(is_home_title))
            .count()
    } else {
        report.warnings.push(format!(
            "Legacy Explorer restore could not inspect current folder windows: {}",
            current
                .message
                .unwrap_or_else(|| "Shell.Application discovery is unavailable".to_owned())
        ));
        0
    };

    report.planned = saved_home_windows.saturating_sub(current_home_windows);
    if report.planned == 0 {
        return report;
    }

    report.warnings.push(format!(
        "Legacy Explorer restore: this capsule predates folder-target capture; {} saved Home/Quick access window(s) can be restored safely, but arbitrary folder windows cannot be reconstructed without their saved paths",
        report.planned
    ));

    if dry_run {
        return report;
    }

    for index in 0..report.planned {
        match Command::new("explorer.exe")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => report.opened += 1,
            Err(error) => report.failures.push(format!(
                "Legacy Explorer Home window could not be opened: {error}"
            )),
        }
        if index + 1 < report.planned {
            thread::sleep(LAUNCH_SPACING);
        }
    }

    report
}

fn saved_legacy_home_window_count(snapshot: &Value) -> usize {
    snapshot
        .pointer("/desktop/applications")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|application| is_explorer_application(application))
        .flat_map(|application| {
            application
                .get("windows")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter(|window| {
            window
                .get("title")
                .and_then(Value::as_str)
                .is_some_and(is_home_title)
        })
        .count()
}

fn is_explorer_application(application: &Value) -> bool {
    application
        .get("executable_path")
        .and_then(Value::as_str)
        .or_else(|| {
            application
                .pointer("/launch/target")
                .and_then(Value::as_str)
        })
        .is_some_and(|value| {
            value
                .rsplit(['\\', '/'])
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("explorer.exe"))
        })
        || application
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("explorer"))
}

fn is_home_title(title: &str) -> bool {
    let normalized = title.trim().to_ascii_lowercase();
    let normalized = normalized
        .strip_suffix(" - file explorer")
        .unwrap_or(&normalized)
        .trim();
    normalized == "home" || normalized == "quick access"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_fallback_only_counts_safe_home_style_windows() {
        let snapshot = json!({
            "desktop": {
                "applications": [{
                    "name": "explorer",
                    "executable_path": "C:\\Windows\\explorer.exe",
                    "windows": [
                        { "title": "Home - File Explorer" },
                        { "title": "Quick access - File Explorer" },
                        { "title": "Secret Project - File Explorer" }
                    ]
                }]
            }
        });
        assert_eq!(saved_legacy_home_window_count(&snapshot), 2);
    }

    #[test]
    fn semantic_explorer_capsules_never_use_legacy_fallback() {
        let snapshot = json!({
            "explorer": { "schema_version": 1, "status": "available", "windows": [] },
            "desktop": {
                "applications": [{
                    "name": "explorer",
                    "windows": [{ "title": "Home - File Explorer" }]
                }]
            }
        });
        assert_eq!(restore(&snapshot, true), LegacyExplorerReport::default());
    }

    #[test]
    fn home_title_matching_is_case_insensitive_and_rejects_folder_names() {
        assert!(is_home_title("Home - File Explorer"));
        assert!(is_home_title("QUICK ACCESS"));
        assert!(!is_home_title("Downloads - File Explorer"));
    }
}
