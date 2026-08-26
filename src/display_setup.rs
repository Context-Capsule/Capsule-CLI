use crate::desktop::{DesktopSnapshot, DisplayInfo, Rect};
use serde_json::{Value, json};

pub const DISPLAY_SETUP_SCHEMA_VERSION: u32 = 1;

pub fn capture(desktop: &Result<DesktopSnapshot, String>) -> Value {
    match desktop {
        Ok(desktop) => available_setup(desktop),
        Err(message) => json!({
            "schema_version": DISPLAY_SETUP_SCHEMA_VERSION,
            "status": "unavailable",
            "message": message,
        }),
    }
}

fn available_setup(desktop: &DesktopSnapshot) -> Value {
    let mut displays = desktop.displays.clone();
    displays.sort_by(|left, right| {
        left.bounds
            .left
            .cmp(&right.bounds.left)
            .then_with(|| left.bounds.top.cmp(&right.bounds.top))
            .then_with(|| left.bounds.right.cmp(&right.bounds.right))
            .then_with(|| left.bounds.bottom.cmp(&right.bounds.bottom))
            .then_with(|| left.device_name.cmp(&right.device_name))
    });

    let primary_device = displays
        .iter()
        .find(|display| display.is_primary)
        .map(|display| display.device_name.clone());
    let virtual_bounds = virtual_bounds(&displays);

    json!({
        "schema_version": DISPLAY_SETUP_SCHEMA_VERSION,
        "status": "available",
        "display_count": displays.len(),
        "primary_device": primary_device,
        "virtual_bounds": virtual_bounds.map(rect_value),
        "topology_signature": topology_signature(&displays),
        "device_signature": device_signature(&displays),
        "displays": displays.iter().map(display_value).collect::<Vec<_>>(),
    })
}

fn topology_signature(displays: &[DisplayInfo]) -> String {
    displays
        .iter()
        .map(|display| {
            format!(
                "{}:{},{},{},{}|work:{},{},{},{}|scale:{}|orientation:{}|primary:{}",
                display.relation_to_primary,
                display.bounds.left,
                display.bounds.top,
                display.bounds.right,
                display.bounds.bottom,
                display.work_area.left,
                display.work_area.top,
                display.work_area.right,
                display.work_area.bottom,
                display.scale_percent,
                display.orientation,
                display.is_primary,
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn device_signature(displays: &[DisplayInfo]) -> String {
    displays
        .iter()
        .map(|display| {
            format!(
                "{}@{},{},{},{}",
                display.device_name,
                display.bounds.left,
                display.bounds.top,
                display.bounds.right,
                display.bounds.bottom,
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn virtual_bounds(displays: &[DisplayInfo]) -> Option<Rect> {
    let first = displays.first()?;
    Some(displays.iter().skip(1).fold(first.bounds, |bounds, display| Rect {
        left: bounds.left.min(display.bounds.left),
        top: bounds.top.min(display.bounds.top),
        right: bounds.right.max(display.bounds.right),
        bottom: bounds.bottom.max(display.bounds.bottom),
    }))
}

fn display_value(display: &DisplayInfo) -> Value {
    json!({
        "device_name": display.device_name,
        "bounds": rect_value(display.bounds),
        "work_area": rect_value(display.work_area),
        "is_primary": display.is_primary,
        "scale_percent": display.scale_percent,
        "orientation": display.orientation,
        "relation_to_primary": display.relation_to_primary,
    })
}

fn rect_value(rect: Rect) -> Value {
    json!({
        "left": rect.left,
        "top": rect.top,
        "right": rect.right,
        "bottom": rect.bottom,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(
        name: &str,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        primary: bool,
        scale: u32,
    ) -> DisplayInfo {
        DisplayInfo {
            device_name: name.to_owned(),
            bounds: Rect {
                left,
                top,
                right,
                bottom,
            },
            work_area: Rect {
                left,
                top,
                right,
                bottom: bottom - 40,
            },
            is_primary: primary,
            scale_percent: scale,
            orientation: "landscape",
            relation_to_primary: if primary {
                "primary".to_owned()
            } else if left < 0 {
                "left".to_owned()
            } else {
                "right".to_owned()
            },
        }
    }

    #[test]
    fn display_setup_records_geometry_scale_orientation_and_virtual_bounds() {
        let desktop = DesktopSnapshot {
            displays: vec![
                display("DISPLAY2", -1920, 0, 0, 1080, false, 100),
                display("DISPLAY1", 0, 0, 2560, 1440, true, 125),
            ],
            applications: Vec::new(),
            ignored: Vec::new(),
        };
        let captured = capture(&Ok(desktop));
        assert_eq!(captured["status"], "available");
        assert_eq!(captured["display_count"], 2);
        assert_eq!(captured["primary_device"], "DISPLAY1");
        assert_eq!(captured["virtual_bounds"]["left"], -1920);
        assert_eq!(captured["virtual_bounds"]["right"], 2560);
        assert!(
            captured["topology_signature"]
                .as_str()
                .unwrap_or_default()
                .contains("scale:125")
        );
    }

    #[test]
    fn topology_signature_is_stable_across_discovery_order() {
        let left = display("DISPLAY2", -1920, 0, 0, 1080, false, 100);
        let primary = display("DISPLAY1", 0, 0, 1920, 1080, true, 100);
        let first = capture(&Ok(DesktopSnapshot {
            displays: vec![left.clone(), primary.clone()],
            applications: Vec::new(),
            ignored: Vec::new(),
        }));
        let second = capture(&Ok(DesktopSnapshot {
            displays: vec![primary, left],
            applications: Vec::new(),
            ignored: Vec::new(),
        }));
        assert_eq!(first["topology_signature"], second["topology_signature"]);
        assert_eq!(first["device_signature"], second["device_signature"]);
    }
}
