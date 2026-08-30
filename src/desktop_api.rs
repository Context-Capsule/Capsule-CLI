use crate::{
    browser, chrome, continuation_notes, desktop, diagnostics, diff, discovery,
    discovery::GitState,
    logging,
    persistence::{self, CapsuleStore},
    vscode,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::{path::Path, process::ExitCode, time::Duration};

pub const DESKTOP_API_VERSION: u32 = 1;
const LOG_COMPONENT: &str = "desktop";

#[derive(Debug, Serialize)]
struct Envelope<T: Serialize> {
    api_version: u32,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct CapsuleSummary {
    name: String,
    created_at_unix_ms: i64,
    updated_at_unix_ms: i64,
    schema_version: u32,
    current_revision: u32,
    revision_count: u32,
    applications: usize,
    browser_tabs: usize,
    editor_tabs: usize,
    terminals: usize,
    services: usize,
    docker_containers: usize,
    note: Option<String>,
}

#[derive(Debug, Serialize)]
struct RevisionSummary {
    name: String,
    revision: u32,
    created_at_unix_ms: i64,
    schema_version: u32,
    current: bool,
    note: Option<String>,
}

#[derive(Debug, Serialize)]
struct ServiceSummary {
    service_index: u32,
    source: String,
    host: String,
    shell: String,
    terminal_name: Option<String>,
    profile: Option<String>,
    working_directory: Option<String>,
    command: String,
    pre_start_command: Option<String>,
    restart_policy: String,
}

pub fn run(arguments: Vec<String>) -> ExitCode {
    let result = match arguments.as_slice() {
        [action] if action == "contract" => contract(),
        [action] if action == "overview" => overview(),
        [action] if action == "applications" => applications(),
        [action] if action == "live" => live_workspace(),
        [action] if action == "health" => health(),
        [action] if action == "log-paths" => log_paths(),
        [action, reference] if action == "capsule" => capsule(reference),
        [action, name] if action == "history" => history(name),
        [action, reference] if action == "services" => services(reference),
        [action, before, after] if action == "diff" => capsule_diff(before, after),
        _ => Err(
            "usage: capsule desktop <contract|overview|applications|live|health|log-paths|capsule <ref>|history <name>|services <ref>|diff <before> <after>>"
                .to_owned(),
        ),
    };

    match result {
        Ok(data) => emit(true, Some(data), None),
        Err(error) => {
            logging::error(LOG_COMPONENT, format!("desktop api failed: {error}"));
            emit(false, None::<Value>, Some(error))
        }
    }
}

fn emit<T: Serialize>(ok: bool, data: Option<T>, error: Option<String>) -> ExitCode {
    let envelope = Envelope {
        api_version: DESKTOP_API_VERSION,
        ok,
        data,
        error,
    };
    match serde_json::to_string(&envelope) {
        Ok(encoded) => {
            println!("{encoded}");
            if ok { ExitCode::SUCCESS } else { ExitCode::from(1) }
        }
        Err(error) => {
            eprintln!("desktop API serialization failed: {error}");
            ExitCode::from(1)
        }
    }
}

fn contract() -> Result<Value, String> {
    Ok(json!({
        "api_version": DESKTOP_API_VERSION,
        "cli_version": env!("CARGO_PKG_VERSION"),
        "features": [
            "overview",
            "capsule-details",
            "history",
            "diff",
            "application-discovery",
            "live-workspace",
            "health",
            "services",
            "log-paths"
        ]
    }))
}

fn overview() -> Result<Value, String> {
    let store = CapsuleStore::open_default().map_err(|error| error.to_string())?;
    let listed = store.list().map_err(|error| error.to_string())?;
    let database_path = store.path().to_path_buf();
    let mut capsules = Vec::with_capacity(listed.len());

    for item in listed {
        let reference = format!("{}@{}", item.name, item.current_revision);
        let stored = store.load(&reference).map_err(|error| error.to_string())?;
        let note = continuation_notes::get(&reference)
            .ok()
            .flatten()
            .map(|note| note.message);
        capsules.push(CapsuleSummary {
            name: item.name,
            created_at_unix_ms: item.created_at_unix_ms,
            updated_at_unix_ms: item.updated_at_unix_ms,
            schema_version: item.schema_version,
            current_revision: item.current_revision,
            revision_count: item.revision_count,
            applications: array_len(&stored.snapshot, "/desktop/applications"),
            browser_tabs: browser_tab_count(&stored.snapshot),
            editor_tabs: editor_tab_count(&stored.snapshot),
            terminals: array_len(&stored.snapshot, "/terminals/sessions"),
            services: service_count_at(&database_path, &reference).unwrap_or(0),
            docker_containers: docker_container_count(&stored.snapshot),
            note,
        });
    }

    Ok(json!({ "capsules": capsules }))
}

fn capsule(reference: &str) -> Result<Value, String> {
    let store = CapsuleStore::open_default().map_err(|error| error.to_string())?;
    let stored = store.load(reference).map_err(|error| error.to_string())?;
    let note = continuation_notes::get(reference)
        .map_err(|error| error.to_string())?
        .map(|note| json!({
            "capsule_name": note.capsule_name,
            "revision": note.revision,
            "message": note.message,
            "updated_at_unix_ms": note.updated_at_unix_ms,
        }));
    let services = services_for_path(store.path(), reference)?;
    Ok(json!({
        "reference": reference,
        "stored": stored,
        "note": note,
        "services": services,
    }))
}

fn history(name: &str) -> Result<Value, String> {
    let store = CapsuleStore::open_default().map_err(|error| error.to_string())?;
    let revisions = store.history(name).map_err(|error| error.to_string())?;
    let mut result = Vec::with_capacity(revisions.len());
    for revision in revisions {
        let reference = format!("{}@{}", revision.name, revision.revision);
        let note = continuation_notes::get(&reference)
            .ok()
            .flatten()
            .map(|note| note.message);
        result.push(RevisionSummary {
            name: revision.name,
            revision: revision.revision,
            created_at_unix_ms: revision.created_at_unix_ms,
            schema_version: revision.schema_version,
            current: revision.current,
            note,
        });
    }
    Ok(json!({ "capsule": name, "revisions": result }))
}

fn capsule_diff(before: &str, after: &str) -> Result<Value, String> {
    let store = CapsuleStore::open_default().map_err(|error| error.to_string())?;
    let before_snapshot = store.load(before).map_err(|error| error.to_string())?;
    let after_snapshot = store.load(after).map_err(|error| error.to_string())?;
    let report = diff::diff_snapshots(&before_snapshot, &after_snapshot);
    serde_json::to_value(report).map_err(|error| error.to_string())
}

fn health() -> Result<Value, String> {
    serde_json::to_value(diagnostics::run()).map_err(|error| error.to_string())
}

fn applications() -> Result<Value, String> {
    let discovered = desktop::discover()
        .map_err(|error| format!("application discovery failed: {error}"))?;
    let applications = discovered
        .applications
        .iter()
        .map(|application| json!({
            "name": application.name,
            "primary_pid": application.primary_pid,
            "pids": application.pids,
            "executable_path": application.executable_path,
            "classification": application.classification.as_str(),
            "confidence": application.confidence,
            "window_count": application.windows.len(),
            "background": application.discovered_as_background,
        }))
        .collect::<Vec<_>>();
    let firefox = browser::load_recent_firefox_state()
        .ok()
        .flatten()
        .and_then(|state| serde_json::to_value(state).ok());

    Ok(json!({
        "applications": applications,
        "browsers": { "firefox": firefox },
    }))
}

fn live_workspace() -> Result<Value, String> {
    let discovered = discovery::discover(true, true, true, true)
        .map_err(|error| format!("live discovery failed: {error}"))?;

    let applications = match &discovered.desktop {
        Ok(desktop) => desktop
            .applications
            .iter()
            .map(|application| json!({
                "name": application.name,
                "primary_pid": application.primary_pid,
                "pids": application.pids,
                "executable_path": application.executable_path,
                "classification": application.classification.as_str(),
                "confidence": application.confidence,
                "window_count": application.windows.len(),
                "background": application.discovered_as_background,
            }))
            .collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };

    let desktop_error = discovered.desktop.as_ref().err().cloned();
    let git = match &discovered.git {
        GitState::Context(context) => serde_json::to_value(context).unwrap_or(Value::Null),
        GitState::NotRepository => json!({ "state": "not-repository" }),
        GitState::GitUnavailable => json!({ "state": "unavailable" }),
    };
    let firefox = browser::load_recent_firefox_state()
        .ok()
        .flatten()
        .and_then(|state| serde_json::to_value(state).ok());
    let chrome = chrome::load_recent_chrome_state()
        .ok()
        .flatten()
        .and_then(|state| serde_json::to_value(state).ok());
    let editor = vscode::load_recent_vscode_state()
        .ok()
        .flatten()
        .and_then(|state| serde_json::to_value(state).ok());

    Ok(json!({
        "current_directory": discovered.current_directory.to_string_lossy(),
        "system": {
            "platform": discovered.system.platform,
            "version": discovered.system.version,
            "architecture": discovered.system.architecture,
        },
        "git": git,
        "tools": discovered.tools.iter().map(|tool| json!({
            "name": tool.name,
            "command": tool.command,
            "version": tool.version,
            "executable_path": tool.executable_path,
        })).collect::<Vec<_>>(),
        "applications": applications,
        "desktop_error": desktop_error,
        "terminals": discovered.terminals,
        "docker": discovered.docker,
        "browsers": { "firefox": firefox, "chrome": chrome },
        "editor": editor,
    }))
}

fn services(reference: &str) -> Result<Value, String> {
    let path = persistence::default_database_path().map_err(|error| error.to_string())?;
    let services = services_for_path(&path, reference)?;
    Ok(json!({ "reference": reference, "services": services }))
}

fn services_for_path(path: &Path, reference: &str) -> Result<Vec<ServiceSummary>, String> {
    let parsed = persistence::parse_capsule_reference(reference).map_err(|error| error.to_string())?;
    let connection = open_database(path)?;
    if !table_exists(&connection, "capsule_terminal_services")? {
        return Ok(Vec::new());
    }
    let (capsule_id, revision) = resolve_capsule_revision(&connection, &parsed.name, parsed.revision)?;
    let mut statement = connection
        .prepare(
            "SELECT service_index, source, host, shell, terminal_name, profile, working_directory,\n\
                    command, pre_start_command, restart_policy\n\
             FROM capsule_terminal_services\n\
             WHERE capsule_id = ?1 AND revision = ?2\n\
             ORDER BY service_index ASC",
        )
        .map_err(|error| format!("SQLite error: {error}"))?;
    let rows = statement
        .query_map(params![capsule_id, revision], |row| {
            Ok(ServiceSummary {
                service_index: row.get(0)?,
                source: row.get(1)?,
                host: row.get(2)?,
                shell: row.get(3)?,
                terminal_name: row.get(4)?,
                profile: row.get(5)?,
                working_directory: row.get(6)?,
                command: row.get(7)?,
                pre_start_command: row.get(8)?,
                restart_policy: row.get(9)?,
            })
        })
        .map_err(|error| format!("SQLite error: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("SQLite error: {error}"))
}

fn service_count_at(path: &Path, reference: &str) -> Result<usize, String> {
    Ok(services_for_path(path, reference)?.len())
}

fn open_database(path: &Path) -> Result<Connection, String> {
    let connection = Connection::open(path).map_err(|error| format!("SQLite error: {error}"))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| format!("SQLite error: {error}"))?;
    Ok(connection)
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, String> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
        .map_err(|error| format!("SQLite error: {error}"))
}

fn resolve_capsule_revision(
    connection: &Connection,
    name: &str,
    requested_revision: Option<u32>,
) -> Result<(i64, u32), String> {
    let capsule_id = connection
        .query_row(
            "SELECT id FROM capsules WHERE name = ?1 COLLATE NOCASE",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| format!("SQLite error: {error}"))?
        .ok_or_else(|| format!("capsule '{name}' was not found"))?;
    let revision = match requested_revision {
        Some(revision) => revision,
        None => connection
            .query_row(
                "SELECT COALESCE(MAX(revision), 1) FROM capsule_revisions WHERE capsule_id = ?1",
                [capsule_id],
                |row| row.get::<_, u32>(0),
            )
            .map_err(|error| format!("SQLite error: {error}"))?,
    };
    let exists = connection
        .query_row(
            "SELECT 1 FROM capsule_revisions WHERE capsule_id = ?1 AND revision = ?2",
            params![capsule_id, revision],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| format!("SQLite error: {error}"))?
        .is_some();
    if !exists {
        return Err(format!("capsule '{name}@{revision}' was not found"));
    }
    Ok((capsule_id, revision))
}

fn log_paths() -> Result<Value, String> {
    let components = ["desktop", "services", "cli", "firefox", "chrome"];
    let mut result = serde_json::Map::new();
    for component in components {
        if let Ok(path) = logging::component_log_path(component) {
            result.insert(component.to_owned(), json!(path.to_string_lossy()));
        }
    }
    Ok(Value::Object(result))
}

fn array_len(root: &Value, pointer: &str) -> usize {
    root.pointer(pointer)
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0)
}

fn browser_tab_count(snapshot: &Value) -> usize {
    ["firefox", "chrome"]
        .iter()
        .map(|browser| {
            snapshot
                .pointer(&format!("/browsers/{browser}/windows"))
                .and_then(Value::as_array)
                .map(|windows| {
                    windows
                        .iter()
                        .map(|window| {
                            window.get("tabs").and_then(Value::as_array).map(Vec::len).unwrap_or(0)
                        })
                        .sum::<usize>()
                })
                .unwrap_or(0)
        })
        .sum()
}

fn editor_tab_count(snapshot: &Value) -> usize {
    snapshot
        .pointer("/editors/vscode/tab_groups")
        .or_else(|| snapshot.pointer("/editors/vscode/tabGroups"))
        .and_then(Value::as_array)
        .map(|groups| {
            groups
                .iter()
                .map(|group| group.get("tabs").and_then(Value::as_array).map(Vec::len).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0)
}

fn docker_container_count(snapshot: &Value) -> usize {
    let compose = snapshot
        .pointer("/docker/compose_projects")
        .or_else(|| snapshot.pointer("/docker/composeProjects"))
        .and_then(Value::as_array)
        .map(|projects| {
            projects
                .iter()
                .map(|project| project.get("containers").and_then(Value::as_array).map(Vec::len).unwrap_or(0))
                .sum::<usize>()
        })
        .unwrap_or(0);
    let standalone = snapshot
        .pointer("/docker/standalone_containers")
        .or_else(|| snapshot.pointer("/docker/standaloneContainers"))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    compose + standalone
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_counts_tolerate_missing_sections() {
        let empty = json!({});
        assert_eq!(browser_tab_count(&empty), 0);
        assert_eq!(editor_tab_count(&empty), 0);
        assert_eq!(docker_container_count(&empty), 0);
        assert_eq!(array_len(&empty, "/terminals/sessions"), 0);
    }

    #[test]
    fn snapshot_counts_cover_browser_editor_terminal_and_docker_data() {
        let value = json!({
            "browsers": {
                "firefox": { "windows": [{ "tabs": [{}, {}] }] },
                "chrome": { "windows": [{ "tabs": [{}] }] }
            },
            "editors": { "vscode": { "tab_groups": [{ "tabs": [{}, {}] }] } },
            "terminals": { "sessions": [{}, {}] },
            "docker": {
                "compose_projects": [{ "containers": [{}, {}] }],
                "standalone_containers": [{}]
            }
        });
        assert_eq!(browser_tab_count(&value), 3);
        assert_eq!(editor_tab_count(&value), 2);
        assert_eq!(array_len(&value, "/terminals/sessions"), 2);
        assert_eq!(docker_container_count(&value), 3);
    }

    #[test]
    fn contract_is_versioned() {
        let value = contract().unwrap();
        assert_eq!(value["api_version"], DESKTOP_API_VERSION);
        assert!(value["features"].as_array().is_some_and(|features| !features.is_empty()));
        assert!(value["features"].as_array().is_some_and(|features| {
            features.iter().any(|feature| feature == "application-discovery")
        }));
    }
}
