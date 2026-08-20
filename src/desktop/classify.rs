use super::model::{ApplicationClassification, NormalizedRect, Rect, SnapPosition};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationDecision {
    pub classification: ApplicationClassification,
    pub confidence: u8,
    pub reason: String,
}

pub fn classify_candidate(
    executable: &str,
    window_titles: &[String],
    has_taskbar_candidate: bool,
    known_background_app: bool,
) -> ClassificationDecision {
    let executable_lower = executable.to_ascii_lowercase();
    let stem = executable_lower
        .strip_suffix(".exe")
        .unwrap_or(&executable_lower);

    if known_background_app {
        return ClassificationDecision {
            classification: ApplicationClassification::UserApplication,
            confidence: 100,
            reason: "known restorable background/tray application".to_owned(),
        };
    }

    if executable_lower == "explorer.exe"
        && window_titles
            .iter()
            .any(|title| title.eq_ignore_ascii_case("Program Manager"))
        && window_titles.len() == 1
    {
        return ClassificationDecision {
            classification: ApplicationClassification::ShellComponent,
            confidence: 100,
            reason: "Windows desktop shell window (Program Manager)".to_owned(),
        };
    }

    if is_known_shell_component(&executable_lower) {
        return ClassificationDecision {
            classification: ApplicationClassification::ShellComponent,
            confidence: 98,
            reason: "known Windows shell/system UI component".to_owned(),
        };
    }

    if is_known_helper_process(&executable_lower) {
        return ClassificationDecision {
            classification: ApplicationClassification::ApplicationHelper,
            confidence: 98,
            reason: "known application helper/host process".to_owned(),
        };
    }

    let generic_helper_name = stem.ends_with("broker")
        || stem.ends_with("helper")
        || stem.ends_with("updater")
        || stem.ends_with("update")
        || stem.ends_with("service")
        || stem.ends_with("svc");

    if generic_helper_name
        && !window_titles.is_empty()
        && window_titles.iter().all(|title| {
            let normalized = normalize_title(title);
            normalized == stem || normalized == executable_lower
        })
    {
        return ClassificationDecision {
            classification: ApplicationClassification::ApplicationHelper,
            confidence: 90,
            reason: "broker/helper/service-style process with only a generic self-titled window"
                .to_owned(),
        };
    }

    if has_taskbar_candidate && !window_titles.is_empty() {
        return ClassificationDecision {
            classification: ApplicationClassification::UserApplication,
            confidence: 95,
            reason: "owns a visible, taskbar-eligible top-level window".to_owned(),
        };
    }

    if !window_titles.is_empty() {
        return ClassificationDecision {
            classification: ApplicationClassification::Unknown,
            confidence: 55,
            reason: "owns visible windows, but none look independently taskbar-eligible".to_owned(),
        };
    }

    ClassificationDecision {
        classification: ApplicationClassification::BackgroundService,
        confidence: 75,
        reason: "running process without a user-facing window".to_owned(),
    }
}

pub fn is_known_background_app(executable: &str) -> bool {
    matches!(
        executable.to_ascii_lowercase().as_str(),
        "docker desktop.exe" | "podman desktop.exe"
    )
}

fn is_known_shell_component(executable: &str) -> bool {
    matches!(
        executable,
        "dwm.exe"
            | "searchhost.exe"
            | "startmenuexperiencehost.exe"
            | "shellexperiencehost.exe"
            | "shellhost.exe"
            | "textinputhost.exe"
            | "lockapp.exe"
            | "securityhealthsystray.exe"
    )
}

fn is_known_helper_process(executable: &str) -> bool {
    matches!(
        executable,
        "runtimebroker.exe"
            | "applicationframehost.exe"
            | "conhost.exe"
            | "systemsettingsbroker.exe"
            | "crashpad_handler.exe"
            | "werfault.exe"
    )
}

fn normalize_title(title: &str) -> String {
    title
        .trim()
        .trim_end_matches(".exe")
        .trim()
        .to_ascii_lowercase()
}

pub fn display_relation(display: Rect, primary: Rect, is_primary: bool) -> String {
    if is_primary {
        return "primary".to_owned();
    }

    let (x, y) = display.center();
    let (primary_x, primary_y) = primary.center();
    let dx = x - primary_x;
    let dy = y - primary_y;

    let horizontal = if dx > 0.0 { "right" } else { "left" };
    let vertical = if dy > 0.0 { "below" } else { "above" };

    if dx.abs() > dy.abs() * 1.5 {
        format!("{horizontal}-of-primary")
    } else if dy.abs() > dx.abs() * 1.5 {
        format!("{vertical}-primary")
    } else {
        format!("{vertical}-{horizontal}-of-primary")
    }
}

pub fn detect_snap(bounds: NormalizedRect) -> Option<SnapPosition> {
    detect_snap_with_arrangement(bounds, take_arrangement_hint())
}

fn detect_snap_with_arrangement(
    bounds: NormalizedRect,
    arranged: Option<bool>,
) -> Option<SnapPosition> {
    // A known non-arranged window must never become snapped from geometry
    // alone. On older Windows where IsWindowArranged is unavailable (`None`),
    // preserve the previous strict stock-layout geometry fallback.
    if arranged == Some(false) {
        return None;
    }

    // Stock layouts remain deliberately strict. The old 6% tolerance could
    // turn an ordinary user-sized Notepad/Explorer window into an exact snap
    // slot on restore. ~1.2% absorbs frame/DPI variance without approximating
    // arbitrary user-resized arranged layouts to the nearest stock fraction.
    const TOLERANCE: f64 = 0.012;

    let patterns = [
        (SnapPosition::LeftHalf, 0.0, 0.0, 0.5, 1.0),
        (SnapPosition::RightHalf, 0.5, 0.0, 0.5, 1.0),
        (SnapPosition::TopHalf, 0.0, 0.0, 1.0, 0.5),
        (SnapPosition::BottomHalf, 0.0, 0.5, 1.0, 0.5),
        (SnapPosition::TopLeftQuarter, 0.0, 0.0, 0.5, 0.5),
        (SnapPosition::TopRightQuarter, 0.5, 0.0, 0.5, 0.5),
        (SnapPosition::BottomLeftQuarter, 0.0, 0.5, 0.5, 0.5),
        (SnapPosition::BottomRightQuarter, 0.5, 0.5, 0.5, 0.5),
        (SnapPosition::LeftThird, 0.0, 0.0, 1.0 / 3.0, 1.0),
        (SnapPosition::CenterThird, 1.0 / 3.0, 0.0, 1.0 / 3.0, 1.0),
        (SnapPosition::RightThird, 2.0 / 3.0, 0.0, 1.0 / 3.0, 1.0),
        (SnapPosition::LeftTwoThirds, 0.0, 0.0, 2.0 / 3.0, 1.0),
        (SnapPosition::RightTwoThirds, 1.0 / 3.0, 0.0, 2.0 / 3.0, 1.0),
    ];

    if let Some(position) = patterns
        .into_iter()
        .find(|(_, x, y, width, height)| {
            approx(bounds.x, *x, TOLERANCE)
                && approx(bounds.y, *y, TOLERANCE)
                && approx(bounds.width, *width, TOLERANCE)
                && approx(bounds.height, *height, TOLERANCE)
        })
        .map(|(position, _, _, _, _)| position)
    {
        return Some(position);
    }

    // If Windows itself says the window is arranged, an unmatched rectangle
    // is not a normal window: it is a snap layout whose divider was resized to
    // an arbitrary ratio. Preserve its exact normalized rectangle rather than
    // rounding it into a known fraction.
    (arranged == Some(true)).then_some(SnapPosition::Custom)
}

fn take_arrangement_hint() -> Option<bool> {
    #[cfg(windows)]
    {
        crate::windows_snap::take_last_arrangement_check()
    }

    #[cfg(not(windows))]
    {
        None
    }
}

fn approx(actual: f64, expected: f64, tolerance: f64) -> bool {
    (actual - expected).abs() <= tolerance
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_manager_is_shell_not_app() {
        let decision =
            classify_candidate("explorer.exe", &["Program Manager".to_owned()], true, false);
        assert_eq!(
            decision.classification,
            ApplicationClassification::ShellComponent
        );
    }

    #[test]
    fn explorer_folder_window_is_a_user_app() {
        let decision = classify_candidate("explorer.exe", &["Downloads".to_owned()], true, false);
        assert_eq!(
            decision.classification,
            ApplicationClassification::UserApplication
        );
    }

    #[test]
    fn generic_broker_self_titled_window_is_helper() {
        let decision = classify_candidate(
            "OmApSvcBroker.exe",
            &["OmApSvcBroker".to_owned()],
            true,
            false,
        );
        assert_eq!(
            decision.classification,
            ApplicationClassification::ApplicationHelper
        );
        assert!(decision.reason.contains("broker/helper/service"));
    }

    #[test]
    fn docker_desktop_can_be_discovered_without_window() {
        let decision = classify_candidate("Docker Desktop.exe", &[], false, true);
        assert_eq!(
            decision.classification,
            ApplicationClassification::UserApplication
        );
        assert_eq!(decision.confidence, 100);
    }

    #[test]
    fn detects_common_snap_layouts() {
        assert_eq!(
            detect_snap_with_arrangement(
                NormalizedRect {
                    x: 0.0,
                    y: 0.0,
                    width: 0.5,
                    height: 1.0,
                },
                None,
            ),
            Some(SnapPosition::LeftHalf)
        );
        assert_eq!(
            detect_snap_with_arrangement(
                NormalizedRect {
                    x: 0.334,
                    y: 0.0,
                    width: 0.666,
                    height: 1.0,
                },
                Some(true),
            ),
            Some(SnapPosition::RightTwoThirds)
        );
    }

    #[test]
    fn ordinary_near_half_window_is_not_promoted_to_snap_slot() {
        let bounds = NormalizedRect {
            x: 0.025,
            y: 0.02,
            width: 0.46,
            height: 0.96,
        };
        assert_eq!(detect_snap_with_arrangement(bounds, Some(false)), None);
        assert_eq!(detect_snap_with_arrangement(bounds, None), None);
    }

    #[test]
    fn small_frame_variance_still_counts_as_stock_snap() {
        assert_eq!(
            detect_snap_with_arrangement(
                NormalizedRect {
                    x: 0.004,
                    y: 0.003,
                    width: 0.504,
                    height: 0.994,
                },
                Some(true),
            ),
            Some(SnapPosition::LeftHalf)
        );
    }

    #[test]
    fn arranged_arbitrary_ratio_becomes_custom_snap() {
        assert_eq!(
            detect_snap_with_arrangement(
                NormalizedRect {
                    x: 0.0,
                    y: 0.0,
                    width: 0.217,
                    height: 1.0,
                },
                Some(true),
            ),
            Some(SnapPosition::Custom)
        );
        assert_eq!(
            detect_snap_with_arrangement(
                NormalizedRect {
                    x: 0.217,
                    y: 0.0,
                    width: 0.783,
                    height: 1.0,
                },
                Some(true),
            ),
            Some(SnapPosition::Custom)
        );
    }

    #[test]
    fn arbitrary_geometry_without_arranged_signal_stays_normal() {
        let bounds = NormalizedRect {
            x: 0.12,
            y: 0.11,
            width: 0.71,
            height: 0.77,
        };
        assert_eq!(detect_snap_with_arrangement(bounds, Some(false)), None);
        assert_eq!(detect_snap_with_arrangement(bounds, None), None);
    }

    #[test]
    fn display_relation_handles_diagonal_layout() {
        let primary = Rect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let display = Rect {
            left: 1920,
            top: -2500,
            right: 4480,
            bottom: -1060,
        };
        assert_eq!(
            display_relation(display, primary, false),
            "above-right-of-primary"
        );
    }
}
