use context_capsule::restore::{SavedDesktop, WindowStateSpec};
use serde_json::json;

#[test]
fn current_persisted_desktop_shape_deserializes_into_restore_model() {
    let payload = json!({
        "desktop": {
            "status": "available",
            "displays": [{
                "device_name": "\\\\.\\DISPLAY1",
                "bounds": { "left": 0, "top": 0, "right": 1920, "bottom": 1080 },
                "work_area": { "left": 0, "top": 0, "right": 1920, "bottom": 1040 },
                "is_primary": true,
                "scale_percent": 125,
                "orientation": "landscape",
                "relation_to_primary": "primary"
            }],
            "applications": [{
                "primary_pid": 4242,
                "pids": [4242],
                "parent_pid": 100,
                "name": "Example",
                "executable_path": "C:\\Apps\\Example.exe",
                "app_user_model_id": null,
                "file_version": "1.2.3",
                "classification": "user-application",
                "confidence": 100,
                "classification_reason": "test fixture",
                "launch": { "strategy": "executable", "target": "C:\\Apps\\Example.exe" },
                "windows": [{
                    "title": "Example - Project",
                    "bounds": { "left": 960, "top": 0, "right": 1920, "bottom": 1040 },
                    "restore_bounds": { "left": 250, "top": 150, "right": 1250, "bottom": 850 },
                    "normalized_bounds": { "x": 0.5, "y": 0.0, "width": 0.5, "height": 1.0 },
                    "state": "snapped:right-half",
                    "display_device": "\\\\.\\DISPLAY1",
                    "display_relation": "primary",
                    "display_scale_percent": 125,
                    "is_foreground": true,
                    "z_order": 0,
                    "virtual_desktop_id": "{11111111-2222-3333-4444-555555555555}",
                    "is_on_current_virtual_desktop": true,
                    "taskbar_candidate": true
                }],
                "discovered_as_background": false
            }],
            "ignored": []
        }
    });

    let desktop = SavedDesktop::from_capsule(&payload)
        .expect("desktop schema should deserialize")
        .expect("desktop should be available");
    assert_eq!(desktop.displays.len(), 1);
    assert_eq!(desktop.applications.len(), 1);
    let app = &desktop.applications[0];
    assert_eq!(app.name, "Example");
    assert_eq!(app.windows.len(), 1);
    assert!(matches!(
        app.windows[0].state_spec(),
        WindowStateSpec::Snapped(_)
    ));
    assert_eq!(app.windows[0].z_order, 0);
    assert!(app.windows[0].is_foreground);
}

#[test]
fn unavailable_desktop_does_not_become_a_restore_target() {
    let payload = json!({
        "desktop": {
            "status": "unavailable",
            "message": "desktop discovery was unavailable"
        }
    });
    assert!(SavedDesktop::from_capsule(&payload).unwrap().is_none());
}
