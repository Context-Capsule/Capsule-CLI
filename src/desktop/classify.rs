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
    // This must be deliberately strict. The old 6% tolerance could turn an
    // ordinary user-sized Notepad/Explorer window that merely happened to sit
    // near a half/third layout into an exact Windows snap slot on restore,
    // making it visibly larger than it was when captured. Real Windows snap
    // geometry differs from the idealized fractions by only a small frame/DPI
    // margin, so ~1.2% is enough to absorb borders without classifying normal
    // windows as snapped.
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

    patterns
        .into_iter()
        .find(|(_, x, y, width, height)| {
            approx(bounds.x, *x, TOLERANCE)
                && approx(bounds.y, *y, TOLERANCE)
                && approx(bounds.width, *width, TOLERANCE)
                && approx(bounds.height, *height, TOLERANCE)
        })
        .map(|(position, _, _, _, _)| position)
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
            detect_snap(NormalizedRect {
                x: 0.0,
                y: 0.0,
                width: 0.5,
                height: 1.0
            }),
            Some(SnapPosition::LeftHalf)
        );
        assert_eq!(
            detect_snap(NormalizedRect {
                x: 0.334,
                y: 0.0,
                width: 0.666,
                height: 1.0
            }),
            Some(SnapPosition::RightTwoThirds)
        );
    }

    #[test]
    fn ordinary_near_half_window_is_not_promoted_to_snap_slot() {
        assert_eq!(
            detect_snap(NormalizedRect {
                x: 0.025,
                y: 0.02,
                width: 0.46,
                height: 0.96
            }),
            None
        );
    }

    #[test]
    fn small_frame_variance_still_counts_as_snapped() {
        assert_eq!(
            detect_snap(NormalizedRect {
                x: 0.004,
                y: 0.003,
                width: 0.504,
                height: 0.994
            }),
            Some(SnapPosition::LeftHalf)
        );
    }

    #[test]
    fn custom_geometry_is_not_forced_into_a_snap_slot() {
        assert_eq!(
            detect_snap(NormalizedRect {
                x: 0.12,
                y: 0.11,
                width: 0.71,
                height: 0.77
            }),
            None
        );
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
