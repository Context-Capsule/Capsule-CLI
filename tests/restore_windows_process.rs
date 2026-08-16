#![cfg(windows)]

use context_capsule::restore::{RestoreOptions, restore_snapshot};
use serde_json::{Value, json};
use std::env;

fn single_background_app(name: &str, executable: &str) -> Value {
    json!({
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
    })
}

#[test]
fn already_running_process_is_not_relaunched() {
    let current = env::current_exe().expect("current test executable");
    let executable = current.to_string_lossy().into_owned();
    let name = current
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("restore_windows_process")
        .to_owned();
    let snapshot = single_background_app(&name, &executable);

    let report = restore_snapshot(&snapshot, RestoreOptions { dry_run: false });
    assert!(report.success(), "restore failed: {report:?}");
    assert_eq!(report.desktop.applications_total, 1);
    assert_eq!(report.desktop.applications_already_running, 1);
    assert_eq!(report.desktop.applications_planned_to_launch, 0);
    assert_eq!(report.desktop.applications_launched, 0);
    assert_eq!(report.desktop.applications_failed, 0);
}

#[test]
fn dry_run_plans_missing_process_without_starting_it() {
    let missing = r"C:\ContextCapsuleTest\definitely-missing-restore-target.exe";
    let snapshot = single_background_app("definitely-missing-restore-target", missing);

    let report = restore_snapshot(&snapshot, RestoreOptions { dry_run: true });
    assert!(report.success(), "dry run failed: {report:?}");
    assert_eq!(report.desktop.applications_total, 1);
    assert_eq!(report.desktop.applications_already_running, 0);
    assert_eq!(report.desktop.applications_planned_to_launch, 1);
    assert_eq!(report.desktop.applications_launched, 0);
    assert_eq!(report.desktop.applications_failed, 0);
}
