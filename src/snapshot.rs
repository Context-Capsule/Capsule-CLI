use crate::{
    adapters::terminal::{TerminalHost, TerminalSnapshot},
    desktop::{ApplicationInfo, DesktopSnapshot, DisplayInfo, IgnoredCandidate, Rect, WindowInfo},
    discovery::{DiscoverySnapshot, GitState},
    persistence::{PersistenceError, StoredCapsuleSnapshot},
};
use context_capsule::{browser, explorer, vscode};
use serde_json::{Value, json};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureOptions {
    pub ignored_applications: Vec<String>,
}

impl CaptureOptions {
    fn ignores(&self, application: &ApplicationInfo) -> bool {
        self.ignored_applications
            .iter()
            .any(|selector| application_matches_selector(application, selector))
    }
}

pub fn validate_ignored_applications(
    discovery: &DiscoverySnapshot,
    selectors: &[String],
) -> Result<Vec<String>, String> {
    if selectors.is_empty() {
        return Ok(Vec::new());
    }

    let desktop = discovery.desktop.as_ref().map_err(|error| {
        format!("cannot apply --ignore-app because desktop discovery is unavailable: {error}")
    })?;
    let mut resolved = Vec::new();

    for selector in selectors {
        let selector = selector.trim();
        if selector.is_empty() {
            return Err("--ignore-app requires a non-empty application selector".to_owned());
        }
        let matches = desktop
            .applications
            .iter()
            .filter(|application| application_matches_selector(application, selector))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            let available = desktop
                .applications
                .iter()
                .map(|application| application.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "--ignore-app '{selector}' did not match a captured application{}",
                if available.is_empty() {
                    String::new()
                } else {
                    format!("; currently detected applications: {available}")
                }
            ));
        }
        for application in matches {
            if !resolved
                .iter()
                .any(|name: &String| name.eq_ignore_ascii_case(&application.name))
            {
                resolved.push(application.name.clone());
            }
        }
    }

    Ok(resolved)
}

pub fn capture_snapshot(
    discovery: &DiscoverySnapshot,
) -> Result<StoredCapsuleSnapshot, PersistenceError> {
    capture_snapshot_with_options(discovery, &CaptureOptions::default())
}

pub fn capture_snapshot_with_options(
    discovery: &DiscoverySnapshot,
    options: &CaptureOptions,
) -> Result<StoredCapsuleSnapshot, PersistenceError> {
    let ignored = discovery
        .desktop
        .as_ref()
        .ok()
        .map(|desktop| {
            desktop
                .applications
                .iter()
                .filter(|application| options.ignores(application))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let docker = serde_json::to_value(&discovery.docker)?;
    let terminals = serde_json::to_value(filtered_terminal_snapshot(&discovery.terminals, &ignored))?;
    let explorer = serde_json::to_value(explorer::discover())?;
    let firefox = if ignored.iter().any(|application| is_firefox_family(application)) {
        None
    } else {
        browser::load_recent_firefox_state()
            .ok()
            .flatten()
            .map(serde_json::to_value)
            .transpose()?
    };
    let vscode = if ignored.iter().any(|application| is_vscode(application)) {
        None
    } else {
        vscode::load_recent_vscode_state()
            .ok()
            .flatten()
            .map(serde_json::to_value)
            .transpose()?
    };
    let ignored_names = ignored
        .iter()
        .map(|application| application.name.clone())
        .collect::<Vec<_>>();
    let snapshot = json!({
        "current_directory": discovery.current_directory.to_string_lossy(),
        "system": {
            "platform": discovery.system.platform,
            "version": discovery.system.version,
            "architecture": discovery.system.architecture,
        },
        "git": git_value(&discovery.git),
        "tools": discovery.tools.iter().map(|tool| json!({
            "name": tool.name,
            "command": tool.command,
            "version": tool.version,
            "executable_path": tool.executable_path,
        })).collect::<Vec<_>>(),
        "version_hints": discovery.version_hints.iter().map(|hint| json!({
            "source": hint.source,
            "value": hint.value,
        })).collect::<Vec<_>>(),
        "capture_options": {
            "ignored_applications": ignored_names,
        },
        "desktop": desktop_value(&discovery.desktop, options),
        "explorer": explorer,
        "docker": docker,
        "terminals": terminals,
        "browsers": { "firefox": firefox },
        "editors": { "vscode": vscode },
    });

    Ok(StoredCapsuleSnapshot::new(snapshot))
}

fn filtered_terminal_snapshot(
    original: &TerminalSnapshot,
    ignored: &[&ApplicationInfo],
) -> TerminalSnapshot {
    let mut filtered = original.clone();
    let suppress_windows_terminal = ignored.iter().any(|application| is_windows_terminal(application));
    let suppress_vscode = ignored.iter().any(|application| is_vscode(application));
    let suppress_cursor = ignored.iter().any(|application| is_cursor(application));
    let suppress_wezterm = ignored.iter().any(|application| is_named_or_executable(application, &["wezterm", "wezterm-gui.exe", "wezterm.exe"]));
    let suppress_alacritty = ignored.iter().any(|application| is_named_or_executable(application, &["alacritty", "alacritty.exe"]));
    let suppress_mintty = ignored.iter().any(|application| is_named_or_executable(application, &["mintty", "mintty.exe"]));

    filtered.sessions.retain(|session| match session.host {
        TerminalHost::WindowsTerminal => !suppress_windows_terminal,
        TerminalHost::VisualStudioCode => !suppress_vscode,
        TerminalHost::Cursor => !suppress_cursor,
        TerminalHost::WezTerm => !suppress_wezterm,
        TerminalHost::Alacritty => !suppress_alacritty,
        TerminalHost::Mintty => !suppress_mintty,
        _ => true,
    });
    if suppress_windows_terminal {
        filtered.windows_terminal_layouts.clear();
    }
    filtered
}

fn git_value(state: &GitState) -> Value {
    match state {
        GitState::Context(context) => json!({
            "status": "repository", "repository_root": context.repository_root,
            "remote_origin": context.remote_origin, "branch": context.branch, "head": context.head,
            "dirty": context.dirty, "changed_files": context.changed_files, "stash_count": context.stash_count,
        }),
        GitState::NotRepository => json!({ "status": "not-repository" }),
        GitState::GitUnavailable => json!({ "status": "git-unavailable" }),
    }
}

fn desktop_value(result: &Result<DesktopSnapshot, String>, options: &CaptureOptions) -> Value {
    match result {
        Ok(desktop) => json!({
            "status": "available",
            "displays": desktop.displays.iter().map(display_value).collect::<Vec<_>>(),
            "applications": desktop.applications.iter()
                .filter(|application| !options.ignores(application))
                .map(application_value)
                .collect::<Vec<_>>(),
            "ignored": desktop.ignored.iter().map(ignored_value).collect::<Vec<_>>(),
        }),
        Err(message) => json!({ "status": "unavailable", "message": message }),
    }
}

fn normalized(value: &str) -> String {
    value.trim().replace('/', "\\").to_ascii_lowercase()
}

fn executable_basename(value: &str) -> Option<&str> {
    Path::new(value).file_name()?.to_str()
}

fn executable_stem(value: &str) -> Option<&str> {
    Path::new(value).file_stem()?.to_str()
}

fn application_matches_selector(application: &ApplicationInfo, selector: &str) -> bool {
    let selector = normalized(selector);
    if normalized(&application.name) == selector {
        return true;
    }

    let mut identities = Vec::new();
    if let Some(path) = application.executable_path.as_deref() {
        identities.push(path);
    }
    if let Some(aumid) = application.app_user_model_id.as_deref() {
        identities.push(aumid);
    }
    if let Some(launch) = application.launch.as_ref() {
        identities.push(launch.target.as_str());
    }

    identities.into_iter().any(|identity| {
        if normalized(identity) == selector {
            return true;
        }
        executable_basename(identity)
            .is_some_and(|name| normalized(name) == selector)
            || executable_stem(identity)
                .is_some_and(|name| normalized(name) == selector)
    })
}

fn app_executable_name(application: &ApplicationInfo) -> Option<&str> {
    application
        .executable_path
        .as_deref()
        .or_else(|| application.launch.as_ref().map(|launch| launch.target.as_str()))
        .and_then(executable_basename)
}

fn is_named_or_executable(application: &ApplicationInfo, values: &[&str]) -> bool {
    values.iter().any(|value| application.name.eq_ignore_ascii_case(value))
        || app_executable_name(application).is_some_and(|name| {
            values.iter().any(|value| name.eq_ignore_ascii_case(value))
        })
}

fn is_firefox_family(application: &ApplicationInfo) -> bool {
    is_named_or_executable(
        application,
        &["zen", "Zen Browser", "zen.exe", "firefox", "Mozilla Firefox", "firefox.exe"],
    )
}

fn is_vscode(application: &ApplicationInfo) -> bool {
    is_named_or_executable(
        application,
        &["Visual Studio Code", "VS Code", "Code", "Code.exe"],
    )
}

fn is_cursor(application: &ApplicationInfo) -> bool {
    is_named_or_executable(application, &["Cursor", "Cursor.exe"])
}

fn is_windows_terminal(application: &ApplicationInfo) -> bool {
    application
        .app_user_model_id
        .as_deref()
        .is_some_and(|value| value.to_ascii_lowercase().contains("windowsterminal"))
        || is_named_or_executable(
            application,
            &["Windows Terminal", "WindowsTerminal.exe", "wt.exe"],
        )
}

fn display_value(display: &DisplayInfo) -> Value {
    json!({
        "device_name": display.device_name, "bounds": rect_value(display.bounds),
        "work_area": rect_value(display.work_area), "is_primary": display.is_primary,
        "scale_percent": display.scale_percent, "orientation": display.orientation,
        "relation_to_primary": display.relation_to_primary,
    })
}

fn application_value(application: &ApplicationInfo) -> Value {
    json!({
        "primary_pid": application.primary_pid, "pids": application.pids,
        "parent_pid": application.parent_pid, "name": application.name,
        "executable_path": application.executable_path, "app_user_model_id": application.app_user_model_id,
        "file_version": application.file_version, "classification": application.classification.as_str(),
        "confidence": application.confidence, "classification_reason": application.classification_reason,
        "launch": application.launch.as_ref().map(|launch| json!({ "strategy": launch.strategy.as_str(), "target": launch.target })),
        "windows": application.windows.iter().map(window_value).collect::<Vec<_>>(),
        "discovered_as_background": application.discovered_as_background,
    })
}

fn window_value(window: &WindowInfo) -> Value {
    json!({
        "title": window.title, "bounds": rect_value(window.bounds),
        "restore_bounds": window.restore_bounds.map(rect_value),
        "normalized_bounds": window.normalized_bounds.map(|bounds| json!({ "x": bounds.x, "y": bounds.y, "width": bounds.width, "height": bounds.height })),
        "state": window.state.to_string(), "display_device": window.display_device,
        "display_relation": window.display_relation, "display_scale_percent": window.display_scale_percent,
        "is_foreground": window.is_foreground, "z_order": window.z_order,
        "virtual_desktop_id": window.virtual_desktop_id,
        "is_on_current_virtual_desktop": window.is_on_current_virtual_desktop,
        "taskbar_candidate": window.taskbar_candidate,
    })
}

fn ignored_value(candidate: &IgnoredCandidate) -> Value {
    json!({
        "pid": candidate.pid, "parent_pid": candidate.parent_pid, "executable": candidate.executable,
        "executable_path": candidate.executable_path, "window_title": candidate.window_title,
        "classification": candidate.classification.as_str(), "confidence": candidate.confidence,
        "reason": candidate.reason,
    })
}

fn rect_value(rect: Rect) -> Value {
    json!({ "left": rect.left, "top": rect.top, "right": rect.right, "bottom": rect.bottom })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        adapters::{
            docker::{DockerSnapshot, DockerStatus},
            terminal::{
                ShellKind, TerminalEnvironment, TerminalHistoryPolicy, TerminalSession,
                TerminalSource, TerminalStatus, WorkingDirectorySource,
            },
        },
        desktop::{ApplicationClassification, LaunchSpec, LaunchStrategy},
        discovery::GitState,
        system::SystemInfo,
    };
    use std::path::PathBuf;

    fn test_application(name: &str, executable_path: &str) -> ApplicationInfo {
        ApplicationInfo {
            primary_pid: 1,
            pids: vec![1],
            parent_pid: None,
            name: name.to_owned(),
            executable_path: Some(executable_path.to_owned()),
            app_user_model_id: None,
            file_version: None,
            classification: ApplicationClassification::UserApplication,
            confidence: 100,
            classification_reason: "test".to_owned(),
            launch: Some(LaunchSpec {
                strategy: LaunchStrategy::Executable,
                target: executable_path.to_owned(),
            }),
            windows: Vec::new(),
            discovered_as_background: false,
        }
    }

    fn base_discovery(applications: Vec<ApplicationInfo>, terminals: TerminalSnapshot) -> DiscoverySnapshot {
        DiscoverySnapshot {
            current_directory: PathBuf::from("/workspace"),
            system: SystemInfo {
                platform: "test".to_owned(),
                version: Some("1".to_owned()),
                architecture: "x86_64".to_owned(),
            },
            git: GitState::NotRepository,
            tools: Vec::new(),
            version_hints: Vec::new(),
            desktop: Ok(DesktopSnapshot {
                displays: Vec::new(),
                applications,
                ignored: Vec::new(),
            }),
            docker: DockerSnapshot {
                status: DockerStatus::Available,
                context: Some("test".to_owned()),
                message: None,
                compose_projects: Vec::new(),
                standalone_containers: Vec::new(),
            },
            terminals,
        }
    }

    #[test]
    fn snapshot_envelope_is_versioned_and_contains_resource_slots() {
        let discovery = base_discovery(Vec::new(), TerminalSnapshot::not_requested());
        let stored = capture_snapshot(&discovery).expect("capture snapshot");
        assert_eq!(stored.schema_version, 1);
        assert_eq!(stored.snapshot["docker"]["status"], "available");
        assert_eq!(stored.snapshot["terminals"]["status"], "not-requested");
        assert_eq!(stored.snapshot["terminals"]["history"]["captured"], false);
        assert_eq!(stored.snapshot["git"]["status"], "not-repository");
        assert!(stored.snapshot.get("explorer").is_some());
        assert!(stored.snapshot.pointer("/browsers/firefox").is_some());
        assert!(stored.snapshot.pointer("/editors/vscode").is_some());
    }

    #[test]
    fn ignore_selector_matches_name_executable_and_stem() {
        let app = test_application("Visual Studio Code", r"C:\\Users\\test\\Code.exe");
        assert!(application_matches_selector(&app, "Visual Studio Code"));
        assert!(application_matches_selector(&app, "Code.exe"));
        assert!(application_matches_selector(&app, "code"));
        assert!(application_matches_selector(&app, r"C:\\Users\\test\\Code.exe"));
        assert!(!application_matches_selector(&app, "Visual Studio"));
    }

    #[test]
    fn ignored_application_is_removed_from_desktop_and_owned_terminal_state() {
        let vscode = test_application("Visual Studio Code", r"C:\\Program Files\\Microsoft VS Code\\Code.exe");
        let terminals = TerminalSnapshot {
            status: TerminalStatus::Available,
            message: None,
            windows_terminal_layouts: Vec::new(),
            sessions: vec![TerminalSession {
                sources: vec![TerminalSource::WindowsProcess],
                host: TerminalHost::VisualStudioCode,
                shell: ShellKind::PowerShell,
                shell_executable: None,
                environment: TerminalEnvironment::Windows,
                pid: None,
                parent_pid: None,
                tty: None,
                profile: None,
                title: None,
                working_directory: None,
                working_directory_source: WorkingDirectorySource::Unknown,
                startup_command: None,
                foreground_command: None,
                restart: None,
            }],
            warnings: Vec::new(),
            history: TerminalHistoryPolicy {
                captured: false,
                reason: "test".to_owned(),
            },
        };
        let discovery = base_discovery(vec![vscode], terminals);
        let stored = capture_snapshot_with_options(
            &discovery,
            &CaptureOptions {
                ignored_applications: vec!["Code.exe".to_owned()],
            },
        )
        .expect("capture filtered snapshot");

        assert_eq!(
            stored
                .snapshot
                .pointer("/desktop/applications")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0)
        );
        assert_eq!(
            stored
                .snapshot
                .pointer("/terminals/sessions")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0)
        );
        assert_eq!(
            stored.snapshot.pointer("/capture_options/ignored_applications/0"),
            Some(&Value::String("Visual Studio Code".to_owned()))
        );
    }
}
