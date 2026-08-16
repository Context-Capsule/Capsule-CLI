use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct SavedDesktop {
    pub status: String,
    #[serde(default)]
    pub displays: Vec<SavedDisplay>,
    #[serde(default)]
    pub applications: Vec<SavedApplication>,
}

impl SavedDesktop {
    pub fn from_capsule(snapshot: &Value) -> Result<Option<Self>, String> {
        let Some(value) = snapshot.get("desktop") else {
            return Ok(None);
        };
        let desktop: Self = serde_json::from_value(value.clone())
            .map_err(|error| format!("invalid saved desktop metadata: {error}"))?;
        if desktop.status != "available" {
            return Ok(None);
        }
        Ok(Some(desktop))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SavedDisplay {
    pub device_name: String,
    pub bounds: SavedRect,
    pub work_area: SavedRect,
    pub is_primary: bool,
    pub scale_percent: u32,
    pub orientation: String,
    pub relation_to_primary: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SavedApplication {
    pub name: String,
    pub executable_path: Option<String>,
    pub app_user_model_id: Option<String>,
    pub file_version: Option<String>,
    pub classification: String,
    pub launch: Option<SavedLaunchSpec>,
    #[serde(default)]
    pub windows: Vec<SavedWindow>,
    #[serde(default)]
    pub discovered_as_background: bool,
}

impl SavedApplication {
    pub fn foreground_window(&self) -> Option<&SavedWindow> {
        self.windows.iter().find(|window| window.is_foreground)
    }

    pub fn frontmost_z_order(&self) -> usize {
        self.windows
            .iter()
            .map(|window| window.z_order)
            .min()
            .unwrap_or(usize::MAX)
    }

    pub fn identity_description(&self) -> String {
        if let Some(aumid) = self.app_user_model_id.as_deref() {
            return format!("AUMID {aumid}");
        }
        if let Some(path) = self.executable_path.as_deref() {
            return path.to_owned();
        }
        if let Some(launch) = self.launch.as_ref() {
            return launch.target.clone();
        }
        self.name.clone()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SavedLaunchSpec {
    pub strategy: String,
    pub target: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SavedWindow {
    pub title: String,
    pub bounds: SavedRect,
    pub restore_bounds: Option<SavedRect>,
    pub normalized_bounds: Option<SavedNormalizedRect>,
    pub state: String,
    pub display_device: String,
    pub display_relation: String,
    pub display_scale_percent: u32,
    pub is_foreground: bool,
    pub z_order: usize,
    pub virtual_desktop_id: Option<String>,
    pub is_on_current_virtual_desktop: Option<bool>,
    pub taskbar_candidate: bool,
}

impl SavedWindow {
    pub fn state_spec(&self) -> WindowStateSpec {
        WindowStateSpec::parse(&self.state)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct SavedRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl SavedRect {
    pub fn width(self) -> i32 {
        self.right.saturating_sub(self.left)
    }

    pub fn height(self) -> i32 {
        self.bottom.saturating_sub(self.top)
    }

    pub fn center(self) -> (f64, f64) {
        (
            (self.left as f64 + self.right as f64) / 2.0,
            (self.top as f64 + self.bottom as f64) / 2.0,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct SavedNormalizedRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowStateSpec {
    Normal,
    Minimized,
    Maximized,
    Fullscreen,
    Snapped(SnapSlot),
    Unknown(String),
}

impl WindowStateSpec {
    fn parse(value: &str) -> Self {
        match value {
            "normal" => Self::Normal,
            "minimized" => Self::Minimized,
            "maximized" => Self::Maximized,
            "fullscreen" => Self::Fullscreen,
            other if other.starts_with("snapped:") => {
                let slot = &other["snapped:".len()..];
                SnapSlot::parse(slot)
                    .map(Self::Snapped)
                    .unwrap_or_else(|| Self::Unknown(other.to_owned()))
            }
            other => Self::Unknown(other.to_owned()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapSlot {
    LeftHalf,
    RightHalf,
    TopHalf,
    BottomHalf,
    TopLeftQuarter,
    TopRightQuarter,
    BottomLeftQuarter,
    BottomRightQuarter,
    LeftThird,
    CenterThird,
    RightThird,
    LeftTwoThirds,
    RightTwoThirds,
}

impl SnapSlot {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "left-half" => Self::LeftHalf,
            "right-half" => Self::RightHalf,
            "top-half" => Self::TopHalf,
            "bottom-half" => Self::BottomHalf,
            "top-left-quarter" => Self::TopLeftQuarter,
            "top-right-quarter" => Self::TopRightQuarter,
            "bottom-left-quarter" => Self::BottomLeftQuarter,
            "bottom-right-quarter" => Self::BottomRightQuarter,
            "left-third" => Self::LeftThird,
            "center-third" => Self::CenterThird,
            "right-third" => Self::RightThird,
            "left-two-thirds" => Self::LeftTwoThirds,
            "right-two-thirds" => Self::RightTwoThirds,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetDisplay {
    pub device_name: String,
    pub bounds: SavedRect,
    pub work_area: SavedRect,
    pub is_primary: bool,
    pub relation_to_primary: String,
}

pub fn choose_display<'a>(
    window: &SavedWindow,
    displays: &'a [TargetDisplay],
) -> Option<&'a TargetDisplay> {
    displays
        .iter()
        .find(|display| display.device_name.eq_ignore_ascii_case(&window.display_device))
        .or_else(|| {
            displays.iter().find(|display| {
                !window.display_relation.is_empty()
                    && display.relation_to_primary == window.display_relation
            })
        })
        .or_else(|| displays.iter().find(|display| display.is_primary))
        .or_else(|| displays.first())
}

pub fn target_rect(window: &SavedWindow, display: &TargetDisplay) -> SavedRect {
    match window.state_spec() {
        WindowStateSpec::Fullscreen => display.bounds,
        WindowStateSpec::Snapped(slot) => snap_rect(display.work_area, slot),
        _ => window
            .normalized_bounds
            .and_then(|bounds| normalized_rect(display.work_area, bounds))
            .unwrap_or_else(|| fallback_rect(window, display)),
    }
}

fn normalized_rect(reference: SavedRect, normalized: SavedNormalizedRect) -> Option<SavedRect> {
    if !normalized.x.is_finite()
        || !normalized.y.is_finite()
        || !normalized.width.is_finite()
        || !normalized.height.is_finite()
        || normalized.width <= 0.0
        || normalized.height <= 0.0
        || reference.width() <= 0
        || reference.height() <= 0
    {
        return None;
    }

    let width = (normalized.width * reference.width() as f64).round() as i32;
    let height = (normalized.height * reference.height() as f64).round() as i32;
    let left = reference.left + (normalized.x * reference.width() as f64).round() as i32;
    let top = reference.top + (normalized.y * reference.height() as f64).round() as i32;
    Some(clamp_rect(
        SavedRect {
            left,
            top,
            right: left.saturating_add(width.max(1)),
            bottom: top.saturating_add(height.max(1)),
        },
        reference,
    ))
}

fn fallback_rect(window: &SavedWindow, display: &TargetDisplay) -> SavedRect {
    let source = window.restore_bounds.unwrap_or(window.bounds);
    let source_width = source.width().max(240).min(display.work_area.width().max(240));
    let source_height = source.height().max(160).min(display.work_area.height().max(160));
    clamp_rect(
        SavedRect {
            left: display.work_area.left,
            top: display.work_area.top,
            right: display.work_area.left.saturating_add(source_width),
            bottom: display.work_area.top.saturating_add(source_height),
        },
        display.work_area,
    )
}

pub fn snap_rect(area: SavedRect, slot: SnapSlot) -> SavedRect {
    let width = area.width().max(1);
    let height = area.height().max(1);
    let half_w = width / 2;
    let half_h = height / 2;
    let third_w = width / 3;

    match slot {
        SnapSlot::LeftHalf => rect_xywh(area.left, area.top, half_w, height),
        SnapSlot::RightHalf => rect_xywh(area.left + half_w, area.top, width - half_w, height),
        SnapSlot::TopHalf => rect_xywh(area.left, area.top, width, half_h),
        SnapSlot::BottomHalf => rect_xywh(area.left, area.top + half_h, width, height - half_h),
        SnapSlot::TopLeftQuarter => rect_xywh(area.left, area.top, half_w, half_h),
        SnapSlot::TopRightQuarter => {
            rect_xywh(area.left + half_w, area.top, width - half_w, half_h)
        }
        SnapSlot::BottomLeftQuarter => {
            rect_xywh(area.left, area.top + half_h, half_w, height - half_h)
        }
        SnapSlot::BottomRightQuarter => rect_xywh(
            area.left + half_w,
            area.top + half_h,
            width - half_w,
            height - half_h,
        ),
        SnapSlot::LeftThird => rect_xywh(area.left, area.top, third_w, height),
        SnapSlot::CenterThird => rect_xywh(area.left + third_w, area.top, third_w, height),
        SnapSlot::RightThird => {
            rect_xywh(area.left + 2 * third_w, area.top, width - 2 * third_w, height)
        }
        SnapSlot::LeftTwoThirds => rect_xywh(area.left, area.top, 2 * third_w, height),
        SnapSlot::RightTwoThirds => {
            rect_xywh(area.left + third_w, area.top, width - third_w, height)
        }
    }
}

fn rect_xywh(left: i32, top: i32, width: i32, height: i32) -> SavedRect {
    SavedRect {
        left,
        top,
        right: left.saturating_add(width.max(1)),
        bottom: top.saturating_add(height.max(1)),
    }
}

fn clamp_rect(rect: SavedRect, area: SavedRect) -> SavedRect {
    let width = rect.width().max(1).min(area.width().max(1));
    let height = rect.height().max(1).min(area.height().max(1));
    let max_left = area.right.saturating_sub(width);
    let max_top = area.bottom.saturating_sub(height);
    let left = rect.left.clamp(area.left, max_left.max(area.left));
    let top = rect.top.clamp(area.top, max_top.max(area.top));
    rect_xywh(left, top, width, height)
}

pub fn rect_close(left: SavedRect, right: SavedRect, tolerance: i32) -> bool {
    (left.left - right.left).abs() <= tolerance
        && (left.top - right.top).abs() <= tolerance
        && (left.right - right.right).abs() <= tolerance
        && (left.bottom - right.bottom).abs() <= tolerance
}

pub fn normalize_windows_path(value: &str) -> String {
    let normalized = value.trim().replace('/', "\\");
    normalized
        .strip_prefix(r"\\?\")
        .unwrap_or(&normalized)
        .to_ascii_lowercase()
}

pub fn title_match_score(saved: &str, current: &str) -> i32 {
    let saved = saved.trim();
    let current = current.trim();
    if saved == current {
        return 100;
    }
    if saved.eq_ignore_ascii_case(current) {
        return 95;
    }
    let saved_lower = saved.to_ascii_lowercase();
    let current_lower = current.to_ascii_lowercase();
    if saved_lower.len() >= 4
        && current_lower.len() >= 4
        && (saved_lower.contains(&current_lower) || current_lower.contains(&saved_lower))
    {
        return 60;
    }

    let saved_tokens = title_tokens(&saved_lower);
    let current_tokens = title_tokens(&current_lower);
    let common = saved_tokens
        .iter()
        .filter(|token| current_tokens.contains(token))
        .count();
    if common == 0 {
        0
    } else {
        ((common * 40) / saved_tokens.len().max(current_tokens.len()).max(1)) as i32
    }
}

fn title_tokens(value: &str) -> Vec<&str> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| token.len() >= 3)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display() -> TargetDisplay {
        TargetDisplay {
            device_name: r"\\.\DISPLAY1".to_owned(),
            bounds: SavedRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            work_area: SavedRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            },
            is_primary: true,
            relation_to_primary: "primary".to_owned(),
        }
    }

    fn window(state: &str) -> SavedWindow {
        SavedWindow {
            title: "Editor - project".to_owned(),
            bounds: SavedRect {
                left: 100,
                top: 100,
                right: 900,
                bottom: 700,
            },
            restore_bounds: None,
            normalized_bounds: Some(SavedNormalizedRect {
                x: 0.25,
                y: 0.25,
                width: 0.5,
                height: 0.5,
            }),
            state: state.to_owned(),
            display_device: r"\\.\DISPLAY1".to_owned(),
            display_relation: "primary".to_owned(),
            display_scale_percent: 100,
            is_foreground: false,
            z_order: 1,
            virtual_desktop_id: None,
            is_on_current_virtual_desktop: Some(true),
            taskbar_candidate: true,
        }
    }

    #[test]
    fn snapped_geometry_is_recomputed_from_current_work_area() {
        let actual = target_rect(&window("snapped:right-two-thirds"), &display());
        assert_eq!(actual.left, 640);
        assert_eq!(actual.top, 0);
        assert_eq!(actual.right, 1920);
        assert_eq!(actual.bottom, 1040);
    }

    #[test]
    fn normalized_geometry_tracks_resolution_changes() {
        let actual = target_rect(&window("normal"), &display());
        assert_eq!(
            actual,
            SavedRect {
                left: 480,
                top: 260,
                right: 1440,
                bottom: 780,
            }
        );
    }

    #[test]
    fn missing_monitor_falls_back_by_relation_then_primary() {
        let mut saved = window("normal");
        saved.display_device = r"\\.\DISPLAY9".to_owned();
        assert_eq!(
            choose_display(&saved, &[display()]).unwrap().device_name,
            r"\\.\DISPLAY1"
        );
    }

    #[test]
    fn title_matching_prefers_exact_then_related_titles() {
        assert_eq!(title_match_score("README.md - Code", "README.md - Code"), 100);
        assert!(title_match_score("README.md - Code", "README.md - Visual Studio Code") > 0);
        assert_eq!(title_match_score("README.md", "Settings"), 0);
    }

    #[test]
    fn windows_paths_are_compared_case_insensitively_and_without_device_prefix() {
        assert_eq!(
            normalize_windows_path(r"\\?\C:\Program Files\App\APP.exe"),
            normalize_windows_path(r"c:/program files/app/app.exe")
        );
    }
}
