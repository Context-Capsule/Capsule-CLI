use crate::{
    desktop::{ApplicationInfo, DesktopSnapshot, DisplayInfo, IgnoredCandidate, Rect, WindowInfo},
    discovery::{DiscoverySnapshot, GitState},
    persistence::{PersistenceError, StoredCapsuleSnapshot},
};
use context_capsule::browser;
use serde_json::{Value, json};

pub fn capture_snapshot(
    discovery: &DiscoverySnapshot,
) -> Result<StoredCapsuleSnapshot, PersistenceError> {
    let docker = serde_json::to_value(&discovery.docker)?;
    let terminals = serde_json::to_value(&discovery.terminals)?;
    let firefox = browser::load_recent_firefox_state()
        .ok()
        .flatten()
        .map(serde_json::to_value)
        .transpose()?;
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
        "desktop": desktop_value(&discovery.desktop),
        "docker": docker,
        "terminals": terminals,
        "browsers": {
            "firefox": firefox,
        },
    });

    Ok(StoredCapsuleSnapshot::new(snapshot))
}

fn git_value(state: &GitState) -> Value {
    match state {
        GitState::Context(context) => json!({
            "status": "repository",
            "repository_root": context.repository_root,
            "remote_origin": context.remote_origin,
            "branch": context.branch,
            "head": context.head,
            "dirty": context.dirty,
            "changed_files": context.changed_files,
            "stash_count": context.stash_count,
        }),
        GitState::NotRepository => json!({ "status": "not-repository" }),
        GitState::GitUnavailable => json!({ "status": "git-unavailable" }),
    }
}

fn desktop_value(result: &Result<DesktopSnapshot, String>) -> Value {
    match result {
        Ok(desktop) => json!({
            "status": "available",
            "displays": desktop.displays.iter().map(display_value).collect::<Vec<_>>(),
            "applications": desktop.applications.iter().map(application_value).collect::<Vec<_>>(),
            "ignored": desktop.ignored.iter().map(ignored_value).collect::<Vec<_>>(),
        }),
        Err(message) => json!({
            "status": "unavailable",
            "message": message,
        }),
    }
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

fn application_value(application: &ApplicationInfo) -> Value {
    json!({
        "primary_pid": application.primary_pid,
        "pids": application.pids,
        "parent_pid": application.parent_pid,
        "name": application.name,
        "executable_path": application.executable_path,
        "app_user_model_id": application.app_user_model_id,
        "file_version": application.file_version,
        "classification": application.classification.as_str(),
        "confidence": application.confidence,
        "classification_reason": application.classification_reason,
        "launch": application.launch.as_ref().map(|launch| json!({
            "strategy": launch.strategy.as_str(),
            "target": launch.target,
        })),
        "windows": application.windows.iter().map(window_value).collect::<Vec<_>>(),
        "discovered_as_background": application.discovered_as_background,
    })
}

fn window_value(window: &WindowInfo) -> Value {
    json!({
        "title": window.title,
        "bounds": rect_value(window.bounds),
        "restore_bounds": window.restore_bounds.map(rect_value),
        "normalized_bounds": window.normalized_bounds.map(|bounds| json!({
            "x": bounds.x,
            "y": bounds.y,
            "width": bounds.width,
            "height": bounds.height,
        })),
        "state": window.state.to_string(),
        "display_device": window.display_device,
        "display_relation": window.display_relation,
        "display_scale_percent": window.display_scale_percent,
        "is_foreground": window.is_foreground,
        "z_order": window.z_order,
        "virtual_desktop_id": window.virtual_desktop_id,
        "is_on_current_virtual_desktop": window.is_on_current_virtual_desktop,
        "taskbar_candidate": window.taskbar_candidate,
    })
}

fn ignored_value(candidate: &IgnoredCandidate) -> Value {
    json!({
        "pid": candidate.pid,
        "parent_pid": candidate.parent_pid,
        "executable": candidate.executable,
        "executable_path": candidate.executable_path,
        "window_title": candidate.window_title,
        "classification": candidate.classification.as_str(),
        "confidence": candidate.confidence,
        "reason": candidate.reason,
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
    use crate::{
        adapters::{
            docker::{DockerSnapshot, DockerStatus},
            terminal::TerminalSnapshot,
        },
        discovery::GitState,
        system::SystemInfo,
    };
    use std::path::PathBuf;

    #[test]
    fn snapshot_envelope_is_versioned_and_contains_resource_slots() {
        let discovery = DiscoverySnapshot {
            current_directory: PathBuf::from("/workspace"),
            system: SystemInfo {
                platform: "test".to_owned(),
                version: Some("1".to_owned()),
                architecture: "x86_64".to_owned(),
            },
            git: GitState::NotRepository,
            tools: Vec::new(),
            version_hints: Vec::new(),
            desktop: Err("not requested".to_owned()),
            docker: DockerSnapshot {
                status: DockerStatus::Available,
                context: Some("test".to_owned()),
                message: None,
                compose_projects: Vec::new(),
                standalone_containers: Vec::new(),
            },
            terminals: TerminalSnapshot::not_requested(),
        };

        let stored = capture_snapshot(&discovery).expect("capture snapshot");
        assert_eq!(stored.schema_version, 1);
        assert_eq!(stored.snapshot["docker"]["status"], "available");
        assert_eq!(stored.snapshot["terminals"]["status"], "not-requested");
        assert_eq!(stored.snapshot["terminals"]["history"]["captured"], false);
        assert_eq!(stored.snapshot["git"]["status"], "not-repository");
        assert!(stored.snapshot.pointer("/browsers/firefox").is_some());
    }
}
