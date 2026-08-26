use crate::persistence::StoredCapsuleSnapshot;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiffKind {
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffChange {
    pub kind: DiffKind,
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffSection {
    pub name: String,
    pub changes: Vec<DiffChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapsuleDiff {
    pub sections: Vec<DiffSection>,
}

impl CapsuleDiff {
    pub fn is_empty(&self) -> bool {
        self.sections
            .iter()
            .all(|section| section.changes.is_empty())
    }

    pub fn change_count(&self) -> usize {
        self.sections
            .iter()
            .map(|section| section.changes.len())
            .sum()
    }
}

pub fn diff_snapshots(
    before: &StoredCapsuleSnapshot,
    after: &StoredCapsuleSnapshot,
) -> CapsuleDiff {
    let mut sections = Vec::new();

    push_section(
        &mut sections,
        "Workspace",
        workspace_changes(&before.snapshot, &after.snapshot),
    );
    push_section(
        &mut sections,
        "Git",
        git_changes(&before.snapshot, &after.snapshot),
    );
    push_section(
        &mut sections,
        "Browser",
        browser_changes(&before.snapshot, &after.snapshot),
    );
    push_section(
        &mut sections,
        "Editor",
        editor_changes(&before.snapshot, &after.snapshot),
    );
    push_section(
        &mut sections,
        "Terminals",
        terminal_changes(&before.snapshot, &after.snapshot),
    );
    push_section(
        &mut sections,
        "Docker",
        docker_changes(&before.snapshot, &after.snapshot),
    );
    push_section(
        &mut sections,
        "Desktop",
        desktop_changes(&before.snapshot, &after.snapshot),
    );
    push_section(
        &mut sections,
        "Tools",
        tool_changes(&before.snapshot, &after.snapshot),
    );

    CapsuleDiff { sections }
}

fn push_section(sections: &mut Vec<DiffSection>, name: &str, changes: Vec<DiffChange>) {
    if !changes.is_empty() {
        sections.push(DiffSection {
            name: name.to_owned(),
            changes,
        });
    }
}

fn workspace_changes(before: &Value, after: &Value) -> Vec<DiffChange> {
    let mut changes = Vec::new();
    compare_scalar(
        &mut changes,
        "current directory",
        string_at(before, "/current_directory"),
        string_at(after, "/current_directory"),
    );
    compare_scalar(
        &mut changes,
        "platform",
        string_at(before, "/system/platform"),
        string_at(after, "/system/platform"),
    );
    compare_scalar(
        &mut changes,
        "architecture",
        string_at(before, "/system/architecture"),
        string_at(after, "/system/architecture"),
    );
    changes
}

fn git_changes(before: &Value, after: &Value) -> Vec<DiffChange> {
    let mut changes = Vec::new();
    for (label, pointer) in [
        ("status", "/git/status"),
        ("repository", "/git/repository_root"),
        ("remote", "/git/remote_origin"),
        ("branch", "/git/branch"),
        ("HEAD", "/git/head"),
        ("dirty", "/git/dirty"),
        ("stash count", "/git/stash_count"),
    ] {
        compare_scalar(
            &mut changes,
            label,
            display_value(before.pointer(pointer)),
            display_value(after.pointer(pointer)),
        );
    }
    compare_sets(
        &mut changes,
        "changed file",
        string_array(before.pointer("/git/changed_files")),
        string_array(after.pointer("/git/changed_files")),
    );
    changes
}

fn browser_changes(before: &Value, after: &Value) -> Vec<DiffChange> {
    let before_browser = before.pointer("/browsers/firefox");
    let after_browser = after.pointer("/browsers/firefox");
    let mut changes = Vec::new();

    compare_scalar(
        &mut changes,
        "adapter",
        browser_presence(before_browser),
        browser_presence(after_browser),
    );
    compare_multisets(
        &mut changes,
        "tab",
        browser_tabs(before_browser),
        browser_tabs(after_browser),
    );
    compare_sets(
        &mut changes,
        "tab group",
        browser_groups(before_browser),
        browser_groups(after_browser),
    );
    changes
}

fn editor_changes(before: &Value, after: &Value) -> Vec<DiffChange> {
    let before_editor = before.pointer("/editors/vscode");
    let after_editor = after.pointer("/editors/vscode");
    let mut changes = Vec::new();

    compare_scalar(
        &mut changes,
        "adapter",
        adapter_presence(before_editor),
        adapter_presence(after_editor),
    );
    compare_sets(
        &mut changes,
        "workspace",
        vscode_workspaces(before_editor),
        vscode_workspaces(after_editor),
    );
    compare_multisets(
        &mut changes,
        "tab",
        vscode_tabs(before_editor),
        vscode_tabs(after_editor),
    );
    compare_multisets(
        &mut changes,
        "integrated terminal",
        vscode_terminals(before_editor),
        vscode_terminals(after_editor),
    );
    changes
}

fn terminal_changes(before: &Value, after: &Value) -> Vec<DiffChange> {
    let mut changes = Vec::new();
    compare_scalar(
        &mut changes,
        "capture status",
        string_at(before, "/terminals/status"),
        string_at(after, "/terminals/status"),
    );
    compare_multisets(
        &mut changes,
        "session",
        terminal_sessions(before.pointer("/terminals")),
        terminal_sessions(after.pointer("/terminals")),
    );
    changes
}

fn docker_changes(before: &Value, after: &Value) -> Vec<DiffChange> {
    let mut changes = Vec::new();
    compare_scalar(
        &mut changes,
        "status",
        string_at(before, "/docker/status"),
        string_at(after, "/docker/status"),
    );
    compare_sets(
        &mut changes,
        "compose project",
        docker_compose_projects(before.pointer("/docker")),
        docker_compose_projects(after.pointer("/docker")),
    );
    compare_sets(
        &mut changes,
        "container",
        docker_containers(before.pointer("/docker")),
        docker_containers(after.pointer("/docker")),
    );
    changes
}

fn desktop_changes(before: &Value, after: &Value) -> Vec<DiffChange> {
    let mut changes = Vec::new();
    compare_scalar(
        &mut changes,
        "capture status",
        string_at(before, "/desktop/status"),
        string_at(after, "/desktop/status"),
    );
    compare_multisets(
        &mut changes,
        "application",
        desktop_applications(before.pointer("/desktop")),
        desktop_applications(after.pointer("/desktop")),
    );
    changes
}

fn tool_changes(before: &Value, after: &Value) -> Vec<DiffChange> {
    let before_tools = tools(before.pointer("/tools"));
    let after_tools = tools(after.pointer("/tools"));
    let mut changes = Vec::new();
    let names = before_tools
        .keys()
        .chain(after_tools.keys())
        .cloned()
        .collect::<BTreeSet<_>>();

    for name in names {
        match (before_tools.get(&name), after_tools.get(&name)) {
            (None, Some(version)) => changes.push(DiffChange {
                kind: DiffKind::Added,
                key: name,
                before: None,
                after: Some(version.clone()),
            }),
            (Some(version), None) => changes.push(DiffChange {
                kind: DiffKind::Removed,
                key: name,
                before: Some(version.clone()),
                after: None,
            }),
            (Some(left), Some(right)) if left != right => changes.push(DiffChange {
                kind: DiffKind::Changed,
                key: name,
                before: Some(left.clone()),
                after: Some(right.clone()),
            }),
            _ => {}
        }
    }
    changes
}

fn compare_scalar(
    changes: &mut Vec<DiffChange>,
    key: &str,
    before: Option<String>,
    after: Option<String>,
) {
    if before == after {
        return;
    }
    let kind = match (&before, &after) {
        (None, Some(_)) => DiffKind::Added,
        (Some(_), None) => DiffKind::Removed,
        _ => DiffKind::Changed,
    };
    changes.push(DiffChange {
        kind,
        key: key.to_owned(),
        before,
        after,
    });
}

fn compare_sets(
    changes: &mut Vec<DiffChange>,
    key: &str,
    before: BTreeSet<String>,
    after: BTreeSet<String>,
) {
    for value in before.difference(&after) {
        changes.push(DiffChange {
            kind: DiffKind::Removed,
            key: key.to_owned(),
            before: Some(value.clone()),
            after: None,
        });
    }
    for value in after.difference(&before) {
        changes.push(DiffChange {
            kind: DiffKind::Added,
            key: key.to_owned(),
            before: None,
            after: Some(value.clone()),
        });
    }
}

fn compare_multisets(
    changes: &mut Vec<DiffChange>,
    key: &str,
    before: Vec<String>,
    after: Vec<String>,
) {
    let before = counts(before);
    let after = counts(after);
    let values = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>();

    for value in values {
        let left = before.get(&value).copied().unwrap_or(0);
        let right = after.get(&value).copied().unwrap_or(0);
        if left > right {
            for _ in 0..(left - right) {
                changes.push(DiffChange {
                    kind: DiffKind::Removed,
                    key: key.to_owned(),
                    before: Some(value.clone()),
                    after: None,
                });
            }
        } else if right > left {
            for _ in 0..(right - left) {
                changes.push(DiffChange {
                    kind: DiffKind::Added,
                    key: key.to_owned(),
                    before: None,
                    after: Some(value.clone()),
                });
            }
        }
    }
}

fn counts(values: Vec<String>) -> BTreeMap<String, usize> {
    let mut result = BTreeMap::new();
    for value in values {
        *result.entry(value).or_insert(0) += 1;
    }
    result
}

fn string_at(value: &Value, pointer: &str) -> Option<String> {
    display_value(value.pointer(pointer))
}

fn display_value(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        other => Some(other.to_string()),
    }
}

fn string_array(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn adapter_presence(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::Null) | None => Some("not captured".to_owned()),
        Some(_) => Some("captured".to_owned()),
    }
}

fn browser_presence(value: Option<&Value>) -> Option<String> {
    adapter_presence(value)
}

fn browser_tabs(browser: Option<&Value>) -> Vec<String> {
    browser
        .and_then(|value| value.get("windows"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|window| {
            window
                .get("tabs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter(|tab| tab.get("restorable").and_then(Value::as_bool) != Some(false))
        .filter_map(|tab| tab.get("url").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn browser_groups(browser: Option<&Value>) -> BTreeSet<String> {
    browser
        .and_then(|value| value.get("windows"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|window| {
            window
                .get("groups")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|group| group.get("title").and_then(Value::as_str))
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
        .collect()
}

fn vscode_workspaces(editor: Option<&Value>) -> BTreeSet<String> {
    editor
        .and_then(|value| value.get("workspaceFolders"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|folder| folder.get("uri").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn vscode_tabs(editor: Option<&Value>) -> Vec<String> {
    editor
        .and_then(|value| value.get("tabGroups"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|group| {
            group
                .get("tabs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter(|tab| tab.get("restorable").and_then(Value::as_bool) != Some(false))
        .filter_map(|tab| tab.get("uri").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn vscode_terminals(editor: Option<&Value>) -> Vec<String> {
    editor
        .and_then(|value| value.get("integratedTerminals"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|terminal| terminal.get("restorable").and_then(Value::as_bool) != Some(false))
        .map(|terminal| {
            let name = terminal
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("terminal");
            let cwd = terminal
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or("cwd unknown");
            format!("{name} @ {cwd}")
        })
        .collect()
}

fn terminal_sessions(terminals: Option<&Value>) -> Vec<String> {
    terminals
        .and_then(|value| value.get("sessions"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|session| {
            let shell = session
                .get("shell")
                .and_then(Value::as_str)
                .unwrap_or("unknown-shell");
            let cwd = session
                .get("working_directory")
                .and_then(Value::as_str)
                .unwrap_or("cwd unknown");
            format!("{shell} @ {cwd}")
        })
        .collect()
}

fn docker_compose_projects(docker: Option<&Value>) -> BTreeSet<String> {
    docker
        .and_then(|value| value.get("compose_projects"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|project| project.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn docker_containers(docker: Option<&Value>) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    if let Some(projects) = docker
        .and_then(|value| value.get("compose_projects"))
        .and_then(Value::as_array)
    {
        for project in projects {
            if let Some(containers) = project.get("containers").and_then(Value::as_array) {
                for container in containers {
                    if let Some(name) = container.get("name").and_then(Value::as_str) {
                        result.insert(name.to_owned());
                    }
                }
            }
        }
    }
    if let Some(containers) = docker
        .and_then(|value| value.get("standalone_containers"))
        .and_then(Value::as_array)
    {
        for container in containers {
            if let Some(name) = container.get("name").and_then(Value::as_str) {
                result.insert(name.to_owned());
            }
        }
    }
    result
}

fn desktop_applications(desktop: Option<&Value>) -> Vec<String> {
    desktop
        .and_then(|value| value.get("applications"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|application| {
            let name = application
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown application");
            let executable = application
                .get("executable_path")
                .and_then(Value::as_str)
                .unwrap_or("");
            if executable.is_empty() {
                name.to_owned()
            } else {
                format!("{name} [{executable}]")
            }
        })
        .collect()
}

fn tools(value: Option<&Value>) -> BTreeMap<String, String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?.to_owned();
            let version = tool
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("version unknown")
                .to_owned();
            Some((name, version))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(snapshot: Value) -> StoredCapsuleSnapshot {
        StoredCapsuleSnapshot {
            schema_version: 1,
            captured_at_unix_ms: 1,
            snapshot,
        }
    }

    #[test]
    fn semantic_diff_reports_browser_git_editor_and_tool_changes() {
        let before = stored(serde_json::json!({
            "current_directory": "C:/work/app",
            "system": { "platform": "windows", "architecture": "x86_64" },
            "git": { "status": "repository", "branch": "main", "head": "aaa", "dirty": false, "changed_files": [] },
            "browsers": { "firefox": { "windows": [{ "tabs": [
                { "url": "https://example.com", "restorable": true }
            ], "groups": [] }] } },
            "editors": { "vscode": { "workspaceFolders": [{"uri":"file:///C:/work/app"}], "tabGroups": [{ "tabs": [
                { "uri": "file:///C:/work/app/a.ts", "restorable": true }
            ] }], "integratedTerminals": [] } },
            "terminals": { "status": "available", "sessions": [] },
            "docker": { "status": "available", "compose_projects": [], "standalone_containers": [] },
            "desktop": { "status": "available", "applications": [] },
            "tools": [{ "name": "node", "version": "22.0" }]
        }));
        let after = stored(serde_json::json!({
            "current_directory": "C:/work/app",
            "system": { "platform": "windows", "architecture": "x86_64" },
            "git": { "status": "repository", "branch": "feature", "head": "bbb", "dirty": true, "changed_files": ["src/a.ts"] },
            "browsers": { "firefox": { "windows": [{ "tabs": [
                { "url": "https://example.com", "restorable": true },
                { "url": "https://docs.example.com", "restorable": true }
            ], "groups": [{"title":"Research"}] }] } },
            "editors": { "vscode": { "workspaceFolders": [{"uri":"file:///C:/work/app"}], "tabGroups": [{ "tabs": [
                { "uri": "file:///C:/work/app/b.ts", "restorable": true }
            ] }], "integratedTerminals": [] } },
            "terminals": { "status": "available", "sessions": [] },
            "docker": { "status": "available", "compose_projects": [], "standalone_containers": [] },
            "desktop": { "status": "available", "applications": [] },
            "tools": [{ "name": "node", "version": "24.0" }]
        }));

        let diff = diff_snapshots(&before, &after);
        assert!(!diff.is_empty());
        assert!(diff.change_count() >= 8);
        let json = serde_json::to_value(&diff).unwrap();
        assert!(json.to_string().contains("https://docs.example.com"));
        assert!(json.to_string().contains("feature"));
        assert!(json.to_string().contains("file:///C:/work/app/b.ts"));
        assert!(json.to_string().contains("24.0"));
    }

    #[test]
    fn duplicate_tabs_are_compared_as_a_multiset() {
        let before = stored(serde_json::json!({
            "browsers": { "firefox": { "windows": [{ "tabs": [
                { "url": "https://example.com", "restorable": true },
                { "url": "https://example.com", "restorable": true }
            ], "groups": [] }] } }
        }));
        let after = stored(serde_json::json!({
            "browsers": { "firefox": { "windows": [{ "tabs": [
                { "url": "https://example.com", "restorable": true }
            ], "groups": [] }] } }
        }));
        let diff = diff_snapshots(&before, &after);
        let browser = diff
            .sections
            .iter()
            .find(|section| section.name == "Browser")
            .unwrap();
        assert_eq!(
            browser
                .changes
                .iter()
                .filter(|change| change.kind == DiffKind::Removed && change.key == "tab")
                .count(),
            1
        );
    }

    #[test]
    fn identical_snapshots_have_no_semantic_diff() {
        let snapshot = stored(serde_json::json!({
            "current_directory": "/work",
            "browsers": { "firefox": null },
            "editors": { "vscode": null }
        }));
        assert!(diff_snapshots(&snapshot, &snapshot).is_empty());
    }
}
