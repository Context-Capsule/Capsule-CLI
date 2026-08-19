use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

#[cfg(windows)]
use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub const EXPLORER_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

#[cfg(windows)]
const EXPLORER_LAUNCH_SPACING: Duration = Duration::from_millis(120);
#[cfg(windows)]
const EXPLORER_NAVIGATION_TIMEOUT: Duration = Duration::from_secs(4);
#[cfg(windows)]
const EXPLORER_NAVIGATION_POLL: Duration = Duration::from_millis(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExplorerStatus {
    Available,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplorerWindowSnapshot {
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplorerSnapshot {
    pub schema_version: u32,
    pub status: ExplorerStatus,
    #[serde(default)]
    pub windows: Vec<ExplorerWindowSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ExplorerSnapshot {
    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            schema_version: EXPLORER_SNAPSHOT_SCHEMA_VERSION,
            status: ExplorerStatus::Unavailable,
            windows: Vec::new(),
            message: Some(message.into()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExplorerRestoreReport {
    pub saved: usize,
    pub already_open: usize,
    pub planned_to_navigate: usize,
    pub navigated: usize,
    pub planned_to_open: usize,
    pub opened: usize,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

pub fn discover() -> ExplorerSnapshot {
    #[cfg(windows)]
    {
        discover_windows()
    }

    #[cfg(not(windows))]
    {
        ExplorerSnapshot::unavailable("Explorer folder capture is available on Windows only")
    }
}

pub fn restore_from_capsule(snapshot: &Value, dry_run: bool) -> ExplorerRestoreReport {
    let mut report = ExplorerRestoreReport::default();
    let Some(value) = snapshot.get("explorer") else {
        let legacy_count = legacy_explorer_window_count(snapshot);
        if legacy_count > 0 {
            report.warnings.push(format!(
                "Explorer restore: this capsule contains {legacy_count} Explorer window(s) but predates folder-target capture; re-save the capsule with the updated CLI to make those folders restorable"
            ));
        }
        return report;
    };

    let saved: ExplorerSnapshot = match serde_json::from_value(value.clone()) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            report
                .failures
                .push(format!("Explorer restore metadata is invalid: {error}"));
            return report;
        }
    };
    if saved.schema_version != EXPLORER_SNAPSHOT_SCHEMA_VERSION {
        report.failures.push(format!(
            "Explorer restore schema {} is unsupported; expected {}",
            saved.schema_version, EXPLORER_SNAPSHOT_SCHEMA_VERSION
        ));
        return report;
    }
    if saved.status != ExplorerStatus::Available {
        if let Some(message) = saved.message {
            report
                .warnings
                .push(format!("Explorer capture was unavailable: {message}"));
        }
        return report;
    }

    report.saved = saved.windows.len();
    if saved.windows.is_empty() {
        return report;
    }

    let current = discover();
    if current.status != ExplorerStatus::Available {
        report.failures.push(format!(
            "Explorer restore could not inspect current folder windows: {}",
            current
                .message
                .unwrap_or_else(|| "Shell.Application discovery is unavailable".to_owned())
        ));
        return report;
    }

    let mut used = HashSet::new();
    let mut missing = Vec::new();
    for saved_window in &saved.windows {
        if let Some(index) = current
            .windows
            .iter()
            .enumerate()
            .find(|(index, candidate)| {
                !used.contains(index)
                    && explorer_targets_equal(&saved_window.target, &candidate.target)
            })
            .map(|(index, _)| index)
        {
            used.insert(index);
            report.already_open += 1;
        } else {
            missing.push(saved_window);
        }
    }

    let unmatched_current = current
        .windows
        .iter()
        .enumerate()
        .filter(|(index, _)| !used.contains(index))
        .map(|(_, window)| window)
        .collect::<Vec<_>>();

    // A changed Explorer location is the same user-facing resource, not a reason
    // to manufacture another window. Reconcile in place only when ownership is
    // unambiguous: exactly one saved folder is missing and exactly one current
    // Explorer folder is unmatched. With multiple candidates Context Capsule
    // remains additive instead of guessing which user window to mutate.
    if should_navigate_in_place(missing.len(), unmatched_current.len()) {
        report.planned_to_navigate = 1;
        if dry_run {
            return report;
        }

        #[cfg(windows)]
        {
            let from = &unmatched_current[0].target;
            let target = &missing[0].target;
            match navigate_existing_target(from, target) {
                Ok(()) if wait_for_target(target, EXPLORER_NAVIGATION_TIMEOUT) => {
                    report.navigated = 1;
                    return report;
                }
                Ok(()) => {
                    report.failures.push(format!(
                        "Explorer '{}' accepted navigation to '{}' but the saved target was not observed within {} ms",
                        from,
                        target,
                        EXPLORER_NAVIGATION_TIMEOUT.as_millis()
                    ));
                    return report;
                }
                Err(error) => {
                    report.failures.push(format!(
                        "Explorer '{}' could not be navigated back to '{}': {error}",
                        from, target
                    ));
                    return report;
                }
            }
        }

        #[cfg(not(windows))]
        {
            report
                .warnings
                .push("Explorer in-place navigation is available on Windows only".to_owned());
            return report;
        }
    }

    report.planned_to_open = missing.len();
    if dry_run || missing.is_empty() {
        return report;
    }

    #[cfg(windows)]
    {
        let total = missing.len();
        for (index, window) in missing.into_iter().enumerate() {
            match launch_target(&window.target) {
                Ok(()) => report.opened += 1,
                Err(error) => report.failures.push(format!(
                    "Explorer '{}' could not be opened: {error}",
                    window.target
                )),
            }
            if index + 1 < total {
                thread::sleep(EXPLORER_LAUNCH_SPACING);
            }
        }
    }

    #[cfg(not(windows))]
    {
        let _ = missing;
        report
            .warnings
            .push("Explorer restore is available on Windows only".to_owned());
    }

    report
}

fn should_navigate_in_place(missing_saved: usize, unmatched_current: usize) -> bool {
    missing_saved == 1 && unmatched_current == 1
}

pub fn explorer_targets_equal(left: &str, right: &str) -> bool {
    normalize_target(left) == normalize_target(right)
}

fn normalize_target(value: &str) -> String {
    let trimmed = value.trim().trim_matches('"').replace('/', "\\");
    let without_trailing = if trimmed.len() > 3 {
        trimmed.trim_end_matches('\\')
    } else {
        trimmed.as_str()
    };
    without_trailing.to_ascii_lowercase()
}

fn legacy_explorer_window_count(snapshot: &Value) -> usize {
    snapshot
        .pointer("/desktop/applications")
        .and_then(Value::as_array)
        .map(|applications| {
            applications
                .iter()
                .filter(|application| is_explorer_application(application))
                .map(|application| {
                    application
                        .get("windows")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                })
                .sum()
        })
        .unwrap_or(0)
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

#[cfg(windows)]
#[derive(Debug, Deserialize)]
struct ShellWindowsOutput {
    #[serde(default)]
    windows: Vec<ExplorerWindowSnapshot>,
}

#[cfg(windows)]
fn discover_windows() -> ExplorerSnapshot {
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
$shell = New-Object -ComObject Shell.Application
$rows = @()
foreach ($window in @($shell.Windows())) {
    try {
        $target = ''
        try { $target = [string]$window.Document.Folder.Self.Path } catch {}
        if ([string]::IsNullOrWhiteSpace($target)) {
            try {
                $locationUrl = [string]$window.LocationURL
                if (-not [string]::IsNullOrWhiteSpace($locationUrl)) {
                    $target = ([Uri]$locationUrl).LocalPath
                }
            } catch {}
        }
        if (-not [string]::IsNullOrWhiteSpace($target)) {
            $rows += [pscustomobject]@{
                target = $target
                title = [string]$window.LocationName
            }
        }
    } catch {}
}
[pscustomobject]@{ windows = @($rows) } | ConvertTo-Json -Compress -Depth 4
"#;

    let output = match Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            return ExplorerSnapshot::unavailable(format!(
                "could not start PowerShell Shell.Application discovery: {error}"
            ));
        }
    };

    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return ExplorerSnapshot::unavailable(if error.is_empty() {
            format!("Shell.Application discovery exited with {}", output.status)
        } else {
            error
        });
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let raw = raw.trim().trim_start_matches('\u{feff}');
    let parsed: ShellWindowsOutput = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(error) => {
            return ExplorerSnapshot::unavailable(format!(
                "Shell.Application returned invalid JSON: {error}"
            ));
        }
    };

    ExplorerSnapshot {
        schema_version: EXPLORER_SNAPSHOT_SCHEMA_VERSION,
        status: ExplorerStatus::Available,
        windows: parsed
            .windows
            .into_iter()
            .filter(|window| !window.target.trim().is_empty())
            .collect(),
        message: None,
    }
}

#[cfg(windows)]
fn navigate_existing_target(from: &str, target: &str) -> Result<(), String> {
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
function Normalize-Target([string]$value) {
    $normalized = $value.Trim().Trim('"')
    $normalized = $normalized.Replace('/', '\')
    if ($normalized.Length -gt 3) { $normalized = $normalized.TrimEnd('\') }
    return $normalized.ToLowerInvariant()
}
function Window-Target($window) {
    $value = ''
    try { $value = [string]$window.Document.Folder.Self.Path } catch {}
    if ([string]::IsNullOrWhiteSpace($value)) {
        try {
            $locationUrl = [string]$window.LocationURL
            if (-not [string]::IsNullOrWhiteSpace($locationUrl)) {
                $value = ([Uri]$locationUrl).LocalPath
            }
        } catch {}
    }
    return $value
}
$shell = New-Object -ComObject Shell.Application
$from = Normalize-Target $env:CONTEXT_CAPSULE_EXPLORER_FROM
$target = $env:CONTEXT_CAPSULE_EXPLORER_TARGET
foreach ($window in @($shell.Windows())) {
    try {
        if ((Normalize-Target (Window-Target $window)) -eq $from) {
            $window.Navigate($target)
            exit 0
        }
    } catch {}
}
throw "the unmatched Explorer window could not be found"
"#;

    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ])
        .env("CONTEXT_CAPSULE_EXPLORER_FROM", from)
        .env("CONTEXT_CAPSULE_EXPLORER_TARGET", target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("could not start Explorer navigation helper: {error}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(if stderr.is_empty() {
            format!("navigation helper exited with {}", output.status)
        } else {
            stderr
        })
    }
}

#[cfg(windows)]
fn wait_for_target(target: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = discover_windows();
        if snapshot.status == ExplorerStatus::Available
            && snapshot
                .windows
                .iter()
                .any(|window| explorer_targets_equal(&window.target, target))
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(EXPLORER_NAVIGATION_POLL);
    }
}

#[cfg(windows)]
fn launch_target(target: &str) -> Result<(), String> {
    Command::new("explorer.exe")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn explorer_target_matching_is_case_and_separator_insensitive_on_windows_paths() {
        assert!(explorer_targets_equal(
            r"C:\Users\Dhia\Project\",
            "c:/users/dhia/project"
        ));
        assert!(!explorer_targets_equal(
            r"C:\Users\Dhia\Project",
            r"C:\Users\Dhia\Other"
        ));
    }

    #[test]
    fn explorer_reuses_only_an_unambiguous_single_changed_window() {
        assert!(should_navigate_in_place(1, 1));
        assert!(!should_navigate_in_place(2, 1));
        assert!(!should_navigate_in_place(1, 2));
        assert!(!should_navigate_in_place(0, 1));
    }

    #[test]
    fn legacy_capsule_reports_missing_folder_target_capture() {
        let report = restore_from_capsule(
            &json!({
                "desktop": {
                    "applications": [{
                        "name": "explorer",
                        "executable_path": "C:\\Windows\\explorer.exe",
                        "windows": [{ "title": "Downloads" }, { "title": "Project" }]
                    }]
                }
            }),
            true,
        );
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("predates folder-target capture"));
    }

    #[test]
    fn empty_saved_explorer_snapshot_is_a_noop() {
        let report = restore_from_capsule(
            &json!({
                "explorer": {
                    "schema_version": 1,
                    "status": "available",
                    "windows": []
                }
            }),
            true,
        );
        assert_eq!(report.saved, 0);
        assert!(report.failures.is_empty());
    }
}
