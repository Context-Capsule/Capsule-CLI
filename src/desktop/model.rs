use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl NormalizedRect {
    pub fn from_rect(rect: Rect, reference: Rect) -> Option<Self> {
        let ref_width = reference.width();
        let ref_height = reference.height();
        if ref_width <= 0 || ref_height <= 0 {
            return None;
        }

        Some(Self {
            x: (rect.left - reference.left) as f64 / ref_width as f64,
            y: (rect.top - reference.top) as f64 / ref_height as f64,
            width: rect.width() as f64 / ref_width as f64,
            height: rect.height() as f64 / ref_height as f64,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapPosition {
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
    /// Windows reports the window as arranged, but its current rectangle no
    /// longer matches one of the stock snap fractions. This happens when the
    /// user drags a divider after snapping (for example 20/80 or 27/73).
    Custom,
}

impl SnapPosition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LeftHalf => "left-half",
            Self::RightHalf => "right-half",
            Self::TopHalf => "top-half",
            Self::BottomHalf => "bottom-half",
            Self::TopLeftQuarter => "top-left-quarter",
            Self::TopRightQuarter => "top-right-quarter",
            Self::BottomLeftQuarter => "bottom-left-quarter",
            Self::BottomRightQuarter => "bottom-right-quarter",
            Self::LeftThird => "left-third",
            Self::CenterThird => "center-third",
            Self::RightThird => "right-third",
            Self::LeftTwoThirds => "left-two-thirds",
            Self::RightTwoThirds => "right-two-thirds",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowState {
    Normal,
    Minimized,
    Maximized,
    Fullscreen,
    Snapped(SnapPosition),
}

impl fmt::Display for WindowState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Normal => formatter.write_str("normal"),
            Self::Minimized => formatter.write_str("minimized"),
            Self::Maximized => formatter.write_str("maximized"),
            Self::Fullscreen => formatter.write_str("fullscreen"),
            Self::Snapped(position) => write!(formatter, "snapped:{}", position.as_str()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationClassification {
    UserApplication,
    ApplicationHelper,
    ShellComponent,
    BackgroundService,
    Unknown,
}

impl ApplicationClassification {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserApplication => "user-application",
            Self::ApplicationHelper => "application-helper",
            Self::ShellComponent => "shell-component",
            Self::BackgroundService => "background-service",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchStrategy {
    Executable,
    AppUserModelId,
}

impl LaunchStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Executable => "executable",
            Self::AppUserModelId => "app-user-model-id",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub strategy: LaunchStrategy,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    pub device_name: String,
    pub bounds: Rect,
    pub work_area: Rect,
    pub is_primary: bool,
    pub scale_percent: u32,
    pub orientation: &'static str,
    pub relation_to_primary: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    pub title: String,
    pub bounds: Rect,
    pub restore_bounds: Option<Rect>,
    pub normalized_bounds: Option<NormalizedRect>,
    pub state: WindowState,
    pub display_device: String,
    pub display_relation: String,
    pub display_scale_percent: u32,
    pub is_foreground: bool,
    pub z_order: usize,
    pub virtual_desktop_id: Option<String>,
    pub is_on_current_virtual_desktop: Option<bool>,
    pub taskbar_candidate: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationInfo {
    pub primary_pid: u32,
    pub pids: Vec<u32>,
    pub parent_pid: Option<u32>,
    pub name: String,
    pub executable_path: Option<String>,
    pub app_user_model_id: Option<String>,
    pub file_version: Option<String>,
    pub classification: ApplicationClassification,
    pub confidence: u8,
    pub classification_reason: String,
    pub launch: Option<LaunchSpec>,
    pub windows: Vec<WindowInfo>,
    pub discovered_as_background: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredCandidate {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub executable: String,
    pub executable_path: Option<String>,
    pub window_title: Option<String>,
    pub classification: ApplicationClassification,
    pub confidence: u8,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DesktopSnapshot {
    pub displays: Vec<DisplayInfo>,
    pub applications: Vec<ApplicationInfo>,
    pub ignored: Vec<IgnoredCandidate>,
}

impl DesktopSnapshot {
    pub fn virtual_desktops(&self) -> Vec<(String, Option<bool>, usize)> {
        let mut result: Vec<(String, Option<bool>, usize)> = Vec::new();

        for window in self
            .applications
            .iter()
            .flat_map(|application| application.windows.iter())
        {
            let Some(id) = window.virtual_desktop_id.as_ref() else {
                continue;
            };

            if let Some(existing) = result.iter_mut().find(|entry| entry.0 == *id) {
                existing.2 += 1;
                if window.is_on_current_virtual_desktop == Some(true) {
                    existing.1 = Some(true);
                } else if existing.1.is_none() {
                    existing.1 = window.is_on_current_virtual_desktop;
                }
            } else {
                result.push((id.clone(), window.is_on_current_virtual_desktop, 1));
            }
        }

        result.sort_by(|left, right| left.0.cmp(&right.0));
        result
    }
}
