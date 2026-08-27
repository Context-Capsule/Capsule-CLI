use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RestoreTarget {
    Apps,
    Vscode,
    Firefox,
    Chrome,
    Terminals,
    Git,
    Docker,
    Explorer,
}

const ALL_TARGETS: [RestoreTarget; 8] = [
    RestoreTarget::Apps,
    RestoreTarget::Vscode,
    RestoreTarget::Firefox,
    RestoreTarget::Chrome,
    RestoreTarget::Terminals,
    RestoreTarget::Git,
    RestoreTarget::Docker,
    RestoreTarget::Explorer,
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreSelection {
    targets: BTreeSet<RestoreTarget>,
}

impl RestoreSelection {
    pub fn add_selector_list(&mut self, value: &str) -> Result<(), String> {
        if value.trim().is_empty() {
            return Err("--only requires at least one restore target".to_owned());
        }

        for selector in value.split(',') {
            let selector = selector.trim();
            if selector.is_empty() {
                return Err("--only contains an empty restore target".to_owned());
            }
            self.add_selector(selector)?;
        }
        Ok(())
    }

    pub fn display(&self) -> String {
        self.targets
            .iter()
            .map(|target| target.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }

    pub fn contains(&self, target: RestoreTarget) -> bool {
        self.targets.contains(&target)
    }

    fn add_selector(&mut self, selector: &str) -> Result<(), String> {
        match selector.to_ascii_lowercase().as_str() {
            "apps" | "app" | "applications" | "application" | "desktop" => {
                self.targets.insert(RestoreTarget::Apps);
            }
            "vscode" | "vs-code" => {
                self.targets.insert(RestoreTarget::Vscode);
            }
            "firefox" | "zen" => {
                self.targets.insert(RestoreTarget::Firefox);
            }
            "chrome" => {
                self.targets.insert(RestoreTarget::Chrome);
            }
            "browser" | "browsers" => {
                self.targets.insert(RestoreTarget::Firefox);
                self.targets.insert(RestoreTarget::Chrome);
            }
            "terminal" | "terminals" => {
                self.targets.insert(RestoreTarget::Terminals);
            }
            "git" => {
                self.targets.insert(RestoreTarget::Git);
            }
            "docker" | "containers" => {
                self.targets.insert(RestoreTarget::Docker);
            }
            "explorer" => {
                self.targets.insert(RestoreTarget::Explorer);
            }
            "all" => self.targets.extend(ALL_TARGETS),
            other => {
                return Err(format!(
                    "unknown restore target '{other}'; expected one of apps, vscode, firefox/zen, chrome, browsers, terminals, git, docker, explorer, all"
                ));
            }
        }
        Ok(())
    }
}

impl RestoreTarget {
    fn as_str(self) -> &'static str {
        match self {
            Self::Apps => "apps",
            Self::Vscode => "vscode",
            Self::Firefox => "firefox",
            Self::Chrome => "chrome",
            Self::Terminals => "terminals",
            Self::Git => "git",
            Self::Docker => "docker",
            Self::Explorer => "explorer",
        }
    }
}

pub fn filter_snapshot(snapshot: &Value, selection: &RestoreSelection) -> Value {
    let mut filtered = snapshot.clone();

    filter_git(&mut filtered, selection);
    filter_desktop(&mut filtered, selection);
    filter_vscode(&mut filtered, selection);
    filter_browsers(&mut filtered, selection);
    filter_terminals(&mut filtered, selection);
    filter_docker(&mut filtered, selection);
    filter_explorer(&mut filtered, selection);

    filtered
}

fn filter_git(snapshot: &mut Value, selection: &RestoreSelection) {
    if selection.contains(RestoreTarget::Git) {
        return;
    }
    if let Some(object) = snapshot.as_object_mut() {
        object.remove("git_repositories");
        object.remove("git");
    }
}

fn filter_desktop(snapshot: &mut Value, selection: &RestoreSelection) {
    let Some(applications) = snapshot
        .pointer_mut("/desktop/applications")
        .and_then(Value::as_array_mut)
    else {
        return;
    };

    if selection.contains(RestoreTarget::Apps) {
        return;
    }

    applications.retain(|application| {
        (selection.contains(RestoreTarget::Vscode) && is_vscode_application(application))
            || (selection.contains(RestoreTarget::Firefox)
                && is_firefox_application(application))
            || (selection.contains(RestoreTarget::Chrome) && is_chrome_application(application))
            || (selection.contains(RestoreTarget::Terminals)
                && is_windows_terminal_application(application))
            || (selection.contains(RestoreTarget::Explorer)
                && is_explorer_application(application))
    });
}

fn filter_vscode(snapshot: &mut Value, selection: &RestoreSelection) {
    if selection.contains(RestoreTarget::Vscode) {
        return;
    }
    if let Some(editors) = snapshot.get_mut("editors").and_then(Value::as_object_mut) {
        editors.insert("vscode".to_owned(), Value::Null);
    }
}

fn filter_browsers(snapshot: &mut Value, selection: &RestoreSelection) {
    let Some(browsers) = snapshot.get_mut("browsers").and_then(Value::as_object_mut) else {
        return;
    };
    if !selection.contains(RestoreTarget::Firefox) {
        browsers.insert("firefox".to_owned(), Value::Null);
    }
    if !selection.contains(RestoreTarget::Chrome) {
        browsers.insert("chrome".to_owned(), Value::Null);
    }
}

fn filter_terminals(snapshot: &mut Value, selection: &RestoreSelection) {
    let include_external = selection.contains(RestoreTarget::Terminals);
    let include_vscode = selection.contains(RestoreTarget::Vscode);
    let Some(terminals) = snapshot.get_mut("terminals").and_then(Value::as_object_mut) else {
        return;
    };

    if !include_external && !include_vscode {
        terminals.insert("status".to_owned(), Value::String("not-requested".to_owned()));
        terminals.insert("sessions".to_owned(), Value::Array(Vec::new()));
        terminals.insert("windows_terminal_layouts".to_owned(), Value::Array(Vec::new()));
        return;
    }

    if let Some(sessions) = terminals.get_mut("sessions").and_then(Value::as_array_mut) {
        sessions.retain(|session| {
            let vscode = session.get("host").and_then(Value::as_str) == Some("visual-studio-code");
            (vscode && include_vscode) || (!vscode && include_external)
        });
    }
    if !include_external {
        terminals.insert("windows_terminal_layouts".to_owned(), Value::Array(Vec::new()));
    }
}

fn filter_docker(snapshot: &mut Value, selection: &RestoreSelection) {
    if selection.contains(RestoreTarget::Docker) {
        return;
    }
    if let Some(object) = snapshot.as_object_mut() {
        object.insert(
            "docker".to_owned(),
            json!({
                "status": "not-requested",
                "context": null,
                "message": null,
                "compose_projects": [],
                "standalone_containers": []
            }),
        );
    }
}

fn filter_explorer(snapshot: &mut Value, selection: &RestoreSelection) {
    if selection.contains(RestoreTarget::Explorer) {
        return;
    }
    if let Some(object) = snapshot.as_object_mut() {
        object.insert(
            "explorer".to_owned(),
            json!({
                "schema_version": 1,
                "status": "unavailable",
                "windows": []
            }),
        );
    }
}

fn application_name(application: &Value) -> Option<&str> {
    application.get("name").and_then(Value::as_str)
}

fn application_aumid(application: &Value) -> Option<&str> {
    application
        .get("app_user_model_id")
        .and_then(Value::as_str)
}

fn executable_basename(application: &Value) -> Option<&str> {
    application
        .get("executable_path")
        .and_then(Value::as_str)
        .or_else(|| application.pointer("/launch/target").and_then(Value::as_str))
        .and_then(|value| value.rsplit(['\\', '/']).next())
}

fn is_vscode_application(application: &Value) -> bool {
    executable_basename(application)
        .is_some_and(|name| name.eq_ignore_ascii_case("code.exe") || name.eq_ignore_ascii_case("code"))
        || application_name(application).is_some_and(|name| {
            name.eq_ignore_ascii_case("Visual Studio Code") || name.eq_ignore_ascii_case("VS Code")
        })
}

fn is_firefox_application(application: &Value) -> bool {
    executable_basename(application).is_some_and(|name| {
        name.eq_ignore_ascii_case("zen.exe")
            || name.eq_ignore_ascii_case("zen")
            || name.eq_ignore_ascii_case("firefox.exe")
            || name.eq_ignore_ascii_case("firefox")
    }) || application_name(application).is_some_and(|name| {
        name.eq_ignore_ascii_case("Zen")
            || name.eq_ignore_ascii_case("Zen Browser")
            || name.eq_ignore_ascii_case("Firefox")
            || name.eq_ignore_ascii_case("Mozilla Firefox")
    })
}

fn is_chrome_application(application: &Value) -> bool {
    executable_basename(application).is_some_and(|name| {
        name.eq_ignore_ascii_case("chrome.exe") || name.eq_ignore_ascii_case("chrome")
    }) || application_name(application).is_some_and(|name| {
        name.eq_ignore_ascii_case("Chrome") || name.eq_ignore_ascii_case("Google Chrome")
    })
}

fn is_windows_terminal_application(application: &Value) -> bool {
    application_aumid(application)
        .is_some_and(|value| value.to_ascii_lowercase().contains("windowsterminal"))
        || executable_basename(application).is_some_and(|name| {
            name.eq_ignore_ascii_case("windowsterminal.exe") || name.eq_ignore_ascii_case("wt.exe")
        })
        || application_name(application)
            .is_some_and(|name| name.eq_ignore_ascii_case("Windows Terminal"))
}

fn is_explorer_application(application: &Value) -> bool {
    executable_basename(application)
        .is_some_and(|name| name.eq_ignore_ascii_case("explorer.exe"))
        || application_name(application).is_some_and(|name| name.eq_ignore_ascii_case("Explorer"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        json!({
            "git": { "branch": "main" },
            "git_repositories": { "repositories": [{ "repository_root": "C:/repo", "branch": "main" }] },
            "desktop": {
                "status": "available",
                "displays": [],
                "applications": [
                    { "name": "Visual Studio Code", "executable_path": "C:/Program Files/Microsoft VS Code/Code.exe" },
                    { "name": "Zen Browser", "executable_path": "C:/Program Files/Zen Browser/zen.exe" },
                    { "name": "Google Chrome", "executable_path": "C:/Program Files/Google/Chrome/Application/chrome.exe" },
                    { "name": "Windows Terminal", "executable_path": "C:/Program Files/WindowsApps/WindowsTerminal.exe" },
                    { "name": "Explorer", "executable_path": "C:/Windows/explorer.exe" },
                    { "name": "Notepad", "executable_path": "C:/Windows/notepad.exe" }
                ]
            },
            "editors": { "vscode": { "schema_version": 1, "windows": [1] } },
            "browsers": {
                "firefox": { "schema_version": 1, "windows": [1] },
                "chrome": { "schema_version": 1, "windows": [1] }
            },
            "terminals": {
                "status": "available",
                "sessions": [
                    { "host": "visual-studio-code" },
                    { "host": "windows-terminal" },
                    { "host": "cursor" }
                ],
                "windows_terminal_layouts": [{ "name": "saved" }]
            },
            "docker": {
                "status": "available",
                "context": "desktop-linux",
                "message": null,
                "compose_projects": [{ "name": "demo" }],
                "standalone_containers": []
            },
            "explorer": {
                "schema_version": 1,
                "status": "available",
                "windows": [{ "target": "C:/repo" }]
            }
        })
    }

    fn selection(value: &str) -> RestoreSelection {
        let mut selection = RestoreSelection::default();
        selection.add_selector_list(value).unwrap();
        selection
    }

    #[test]
    fn aliases_and_comma_lists_are_normalized_and_deduplicated() {
        let mut selected = RestoreSelection::default();
        selected.add_selector_list("vscode, terminals,zen").unwrap();
        selected.add_selector_list("terminal,browsers").unwrap();
        assert_eq!(selected.display(), "vscode,firefox,chrome,terminals");
    }

    #[test]
    fn invalid_or_empty_selectors_are_rejected() {
        let mut selected = RestoreSelection::default();
        assert!(selected.add_selector_list("").is_err());
        assert!(selected.add_selector_list("vscode,,git").is_err());
        assert!(selected.add_selector_list("definitely-not-real").is_err());
    }

    #[test]
    fn vscode_only_keeps_editor_integrated_terminals_and_vscode_desktop() {
        let filtered = filter_snapshot(&fixture(), &selection("vscode"));
        let applications = filtered["desktop"]["applications"].as_array().unwrap();
        assert_eq!(applications.len(), 1);
        assert_eq!(applications[0]["name"], "Visual Studio Code");
        assert!(!filtered["editors"]["vscode"].is_null());
        assert!(filtered["browsers"]["firefox"].is_null());
        assert!(filtered["browsers"]["chrome"].is_null());
        let sessions = filtered["terminals"]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["host"], "visual-studio-code");
        assert_eq!(
            filtered["terminals"]["windows_terminal_layouts"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert_eq!(filtered["docker"]["status"], "not-requested");
        assert_eq!(filtered["explorer"]["status"], "unavailable");
        assert!(filtered.get("git").is_none());
        assert!(filtered.get("git_repositories").is_none());
    }

    #[test]
    fn terminals_and_git_keep_external_terminals_without_vscode() {
        let filtered = filter_snapshot(&fixture(), &selection("terminals,git"));
        let applications = filtered["desktop"]["applications"].as_array().unwrap();
        assert_eq!(applications.len(), 1);
        assert_eq!(applications[0]["name"], "Windows Terminal");
        assert!(filtered["editors"]["vscode"].is_null());
        let sessions = filtered["terminals"]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().all(|session| session["host"] != "visual-studio-code"));
        assert!(filtered.get("git").is_some());
        assert!(filtered.get("git_repositories").is_some());
    }

    #[test]
    fn apps_only_preserves_desktop_inventory_but_disables_semantic_resources() {
        let filtered = filter_snapshot(&fixture(), &selection("apps"));
        assert_eq!(filtered["desktop"]["applications"].as_array().unwrap().len(), 6);
        assert!(filtered["editors"]["vscode"].is_null());
        assert!(filtered["browsers"]["firefox"].is_null());
        assert!(filtered["browsers"]["chrome"].is_null());
        assert_eq!(filtered["terminals"]["status"], "not-requested");
        assert_eq!(filtered["docker"]["status"], "not-requested");
        assert_eq!(filtered["explorer"]["status"], "unavailable");
    }

    #[test]
    fn browsers_alias_keeps_both_browser_adapters_and_only_browser_desktop_apps() {
        let filtered = filter_snapshot(&fixture(), &selection("browsers"));
        let names = filtered["desktop"]["applications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|application| application["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Zen Browser", "Google Chrome"]);
        assert!(!filtered["browsers"]["firefox"].is_null());
        assert!(!filtered["browsers"]["chrome"].is_null());
    }

    #[test]
    fn all_selector_keeps_every_known_resource() {
        let original = fixture();
        let filtered = filter_snapshot(&original, &selection("all"));
        assert_eq!(filtered, original);
    }
}
