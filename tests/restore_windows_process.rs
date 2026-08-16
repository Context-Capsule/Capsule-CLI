#![cfg(windows)]

use context_capsule::restore::{RestoreOptions, restore_snapshot};
use serde_json::json;
use std::env;

#[test]
fn already_running_process_is_not_relaunched() {
    let executable = env::current_exe().expect("current test executable");
    let executable = executable.to_string_lossy().into_owned();
    let name = env::current_exe()
        .expect("current test executable")
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("restore_windows_process")
        .to_owned();

    let snapshot = json!({
        "desktop": {
            "status": "available",
            "displays": [],
            "applications": [{
                "name": name,
                "executable_path": executable,
                "app_user_model_id": null,
                "file_version": null,
                "classification": "user-application",
                "launch": {
                    "strategy": "executable",
                    "target": executable
                },
                "windows": [],
                "discovered_as_background": true
            }]
        }
    });

    let report = restore_snapshot(&snapshot, RestoreOptions { dry_run: false });
    assert!(report.success(), "restore failed: {report:?}");
    assert_eq!(report.desktop.applications_total, 1);
    assert_eq!(report.desktop.applications_already_running, 1);
    assert_eq!(report.desktop.applications_planned_to_launch, 0);
    assert_eq!(report.desktop.applications_launched, 0);
    assert_eq!(report.desktop.applications_failed, 0);
}
