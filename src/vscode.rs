use serde::{Deserialize, Serialize};
use std::{
    env,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use std::ffi::c_void;

pub const VSCODE_SNAPSHOT_SCHEMA_VERSION: u32 = 1;
const LIVE_STATE_MAX_AGE: Duration = Duration::from_secs(90);
const MAX_TABS: usize = 100_000;
const MAX_TERMINALS: usize = 10_000;
const MAX_EXTENSION_PATH_LENGTH: usize = 32_768;
const VSCODE_HOST_STATE_PREFIX: &str = "vscode-host-";
const VSCODE_HOST_STATE_SUFFIX: &str = ".json";

#[cfg(windows)]
type Handle = *mut c_void;
#[cfg(windows)]
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> Handle;
    fn CloseHandle(handle: Handle) -> i32;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFolderSnapshot {
    pub uri: String,
    pub name: String,
    pub index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionSnapshot {
    pub anchor: [u32; 2],
    pub active: [u32; 2],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorSelectionSnapshot {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_column: Option<u32>,
    pub selections: Vec<SelectionSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TabSnapshot {
    pub label: String,
    pub input_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    pub active: bool,
    pub dirty: bool,
    pub pinned: bool,
    pub preview: bool,
    pub restorable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TabGroupSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_column: Option<u32>,
    pub active: bool,
    pub tabs: Vec<TabSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IntegratedTerminalShellArgs {
    String(String),
    List(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegratedTerminalSnapshot {
    pub name: String,
    pub kind: String,
    pub restorable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_args: Option<IntegratedTerminalShellArgs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd_is_uri: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VsCodeSnapshot {
    pub schema_version: u32,
    pub captured_at_unix_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_pid: Option<u32>,
    pub app_name: String,
    pub app_host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_detection: Option<String>,
    pub workspace_trusted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_file: Option<String>,
    pub workspace_folders: Vec<WorkspaceFolderSnapshot>,
    pub tab_groups: Vec<TabGroupSnapshot>,
    pub visible_editor_selections: Vec<EditorSelectionSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_editor_uri: Option<String>,
    #[serde(default)]
    pub integrated_terminals: Vec<IntegratedTerminalSnapshot>,
}

impl VsCodeSnapshot {
    pub fn tab_count(&self) -> usize {
        self.tab_groups.iter().map(|group| group.tabs.len()).sum()
    }

    pub fn is_extension_development_host(&self) -> bool {
        self.extension_mode.as_deref() == Some("development") && self.extension_path.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeEnvelope {
    updated_at_unix_ms: i64,
    snapshot: VsCodeSnapshot,
}

#[derive(Debug)]
pub enum VsCodeError {
    Io(io::Error),
    Json(serde_json::Error),
    Invalid(String),
}

impl fmt::Display for VsCodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Json(error) => write!(formatter, "JSON error: {error}"),
            Self::Invalid(message) => write!(formatter, "{message}"),
        }
    }
}

impl Error for VsCodeError {}
impl From<io::Error> for VsCodeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<serde_json::Error> for VsCodeError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub fn load_recent_vscode_state() -> Result<Option<VsCodeSnapshot>, VsCodeError> {
    let canonical = runtime_state_path()?;
    let mut best: Option<RuntimeEnvelope> = None;
    let mut canonical_error: Option<VsCodeError> = None;

    for path in runtime_state_candidates(&canonical) {
        let envelope = match read_runtime_state_at(&path) {
            Ok(envelope) => envelope,
            Err(VsCodeError::Io(error)) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                if path == canonical {
                    canonical_error = Some(error);
                }
                continue;
            }
        };

        let age_ms = now_unix_ms().saturating_sub(envelope.updated_at_unix_ms);
        if age_ms > LIVE_STATE_MAX_AGE.as_millis() as i64 {
            continue;
        }
        if let Err(error) = validate_snapshot(&envelope.snapshot) {
            if path == canonical {
                canonical_error = Some(error);
            }
            continue;
        }
        if !snapshot_host_is_alive(&envelope.snapshot) {
            continue;
        }

        if best
            .as_ref()
            .is_none_or(|current| runtime_envelope_preferred(&envelope, current))
        {
            best = Some(envelope);
        }
    }

    if let Some(envelope) = best {
        return Ok(Some(envelope.snapshot));
    }
    if let Some(error) = canonical_error {
        return Err(error);
    }
    Ok(None)
}

pub fn live_development_host_matches_path(extension_path: &str) -> Result<bool, VsCodeError> {
    Ok(load_recent_vscode_state()?.is_some_and(|snapshot| {
        snapshot.extension_mode.as_deref() == Some("development")
            && snapshot
                .extension_path
                .as_deref()
                .is_some_and(|current| same_development_path(current, extension_path))
    }))
}

fn same_development_path(left: &str, right: &str) -> bool {
    normalize_development_path(left) == normalize_development_path(right)
}

fn normalize_development_path(value: &str) -> String {
    let normalized = value.trim().replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    #[cfg(windows)]
    {
        return normalized.to_ascii_lowercase();
    }
    #[cfg(not(windows))]
    {
        normalized.to_owned()
    }
}

fn snapshot_host_is_alive(snapshot: &VsCodeSnapshot) -> bool {
    snapshot.host_pid.is_none_or(process_is_alive)
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    unsafe {
        CloseHandle(handle);
    }
    true
}

#[cfg(not(windows))]
fn process_is_alive(_pid: u32) -> bool {
    // The Windows product path can cheaply verify extension-host liveness with
    // OpenProcess. Other platforms continue to use the short freshness window
    // until their process-liveness adapter is implemented.
    true
}

pub fn runtime_state_path() -> Result<PathBuf, VsCodeError> {
    if let Some(path) = env::var_os("CONTEXT_CAPSULE_VSCODE_STATE_PATH") {
        if path.is_empty() {
            return Err(VsCodeError::Invalid(
                "CONTEXT_CAPSULE_VSCODE_STATE_PATH is empty".to_owned(),
            ));
        }
        return Ok(PathBuf::from(path));
    }

    #[cfg(windows)]
    {
        let base = env::var_os("LOCALAPPDATA")
            .ok_or_else(|| VsCodeError::Invalid("LOCALAPPDATA is not available".to_owned()))?;
        return Ok(PathBuf::from(base)
            .join("ContextCapsule")
            .join("runtime")
            .join("vscode.json"));
    }
    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME")
            .ok_or_else(|| VsCodeError::Invalid("HOME is not available".to_owned()))?;
        return Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("ContextCapsule")
            .join("runtime")
            .join("vscode.json"));
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        if let Some(base) = env::var_os("XDG_STATE_HOME") {
            return Ok(PathBuf::from(base)
                .join("context-capsule")
                .join("vscode.json"));
        }
        let home = env::var_os("HOME")
            .ok_or_else(|| VsCodeError::Invalid("HOME is not available".to_owned()))?;
        Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("context-capsule")
            .join("vscode.json"))
    }
}

fn runtime_state_candidates(canonical: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![canonical.to_path_buf()];
    let Some(parent) = canonical.parent() else {
        return candidates;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return candidates;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(VSCODE_HOST_STATE_PREFIX) && name.ends_with(VSCODE_HOST_STATE_SUFFIX) {
            candidates.push(entry.path());
        }
    }
    candidates
}

fn read_runtime_state_at(path: &Path) -> Result<RuntimeEnvelope, VsCodeError> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn runtime_envelope_preferred(candidate: &RuntimeEnvelope, current: &RuntimeEnvelope) -> bool {
    let candidate_key = (
        snapshot_priority(&candidate.snapshot),
        candidate.updated_at_unix_ms,
        candidate.snapshot.captured_at_unix_ms,
    );
    let current_key = (
        snapshot_priority(&current.snapshot),
        current.updated_at_unix_ms,
        current.snapshot.captured_at_unix_ms,
    );
    candidate_key > current_key
}

fn snapshot_priority(snapshot: &VsCodeSnapshot) -> u8 {
    match snapshot.extension_mode.as_deref() {
        Some("development") if snapshot.extension_path.is_some() => 4,
        Some("production") => 3,
        Some("test") => 2,
        Some("development") => 1,
        _ => 0,
    }
}

fn validate_snapshot(snapshot: &VsCodeSnapshot) -> Result<(), VsCodeError> {
    if snapshot.schema_version != VSCODE_SNAPSHOT_SCHEMA_VERSION {
        return Err(VsCodeError::Invalid(format!(
            "unsupported VS Code snapshot schema {}; expected {}",
            snapshot.schema_version, VSCODE_SNAPSHOT_SCHEMA_VERSION
        )));
    }
    if snapshot.tab_count() > MAX_TABS {
        return Err(VsCodeError::Invalid(
            "VS Code snapshot contains too many tabs".to_owned(),
        ));
    }
    if snapshot.integrated_terminals.len() > MAX_TERMINALS {
        return Err(VsCodeError::Invalid(
            "VS Code snapshot contains too many integrated terminals".to_owned(),
        ));
    }
    if snapshot
        .extension_path
        .as_ref()
        .is_some_and(|path| path.len() > MAX_EXTENSION_PATH_LENGTH)
    {
        return Err(VsCodeError::Invalid(
            "VS Code extension development path is unreasonably long".to_owned(),
        ));
    }
    Ok(())
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_path() -> PathBuf {
        env::temp_dir().join(format!(
            "context-capsule-vscode-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn snapshot() -> VsCodeSnapshot {
        VsCodeSnapshot {
            schema_version: 1,
            captured_at_unix_ms: now_unix_ms(),
            host_pid: Some(std::process::id()),
            app_name: "Visual Studio Code".to_owned(),
            app_host: "desktop".to_owned(),
            remote_name: Some("wsl".to_owned()),
            extension_mode: Some("development".to_owned()),
            extension_path: Some(r"C:\work\Capsule-VSCode-Extension".to_owned()),
            host_detection: Some("workspace-development-extension".to_owned()),
            workspace_trusted: true,
            workspace_file: None,
            workspace_folders: vec![WorkspaceFolderSnapshot {
                uri: "vscode-remote://wsl+Ubuntu/home/user/project".to_owned(),
                name: "project".to_owned(),
                index: 0,
            }],
            tab_groups: vec![TabGroupSnapshot {
                view_column: Some(1),
                active: true,
                tabs: vec![TabSnapshot {
                    label: "main.rs".to_owned(),
                    input_kind: "text".to_owned(),
                    uri: Some(
                        "vscode-remote://wsl+Ubuntu/home/user/project/src/main.rs".to_owned(),
                    ),
                    active: true,
                    dirty: false,
                    pinned: true,
                    preview: false,
                    restorable: true,
                }],
            }],
            visible_editor_selections: Vec::new(),
            active_editor_uri: None,
            integrated_terminals: vec![IntegratedTerminalSnapshot {
                name: "PowerShell".to_owned(),
                kind: "process".to_owned(),
                restorable: true,
                shell_path: Some("pwsh.exe".to_owned()),
                shell_args: Some(IntegratedTerminalShellArgs::List(vec!["-NoLogo".to_owned()])),
                cwd: Some("file:///C:/work/project".to_owned()),
                cwd_is_uri: Some(true),
            }],
        }
    }

    #[test]
    fn reads_recent_state_and_preserves_remote_uris_and_devhost_identity() {
        let path = test_path();
        let envelope = RuntimeEnvelope {
            updated_at_unix_ms: now_unix_ms(),
            snapshot: snapshot(),
        };
        fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
        let loaded = read_runtime_state_at(&path).unwrap();
        validate_snapshot(&loaded.snapshot).unwrap();
        assert_eq!(loaded.snapshot.remote_name.as_deref(), Some("wsl"));
        assert_eq!(loaded.snapshot.host_detection.as_deref(), Some("workspace-development-extension"));
        assert_eq!(loaded.snapshot.tab_count(), 1);
        assert_eq!(loaded.snapshot.integrated_terminals.len(), 1);
        assert!(loaded.snapshot.is_extension_development_host());
        assert!(snapshot_host_is_alive(&loaded.snapshot));
        fs::remove_file(path).ok();
    }

    #[test]
    fn typescript_sidecar_fields_survive_rust_round_trip() {
        let raw = serde_json::json!({
            "updatedAtUnixMs": now_unix_ms(),
            "snapshot": {
                "schemaVersion": 1,
                "capturedAtUnixMs": now_unix_ms(),
                "hostPid": std::process::id(),
                "appName": "Visual Studio Code",
                "appHost": "desktop",
                "extensionMode": "development",
                "extensionPath": "C:/work/extension",
                "hostDetection": "workspace-development-extension",
                "workspaceTrusted": true,
                "workspaceFolders": [],
                "tabGroups": [],
                "visibleEditorSelections": [],
                "integratedTerminals": [{
                    "name": "PowerShell",
                    "kind": "process",
                    "restorable": true,
                    "shellPath": "pwsh.exe",
                    "shellArgs": ["-NoLogo"],
                    "cwd": "file:///C:/work/extension",
                    "cwdIsUri": true
                }]
            }
        });
        let envelope: RuntimeEnvelope = serde_json::from_value(raw).unwrap();
        let serialized = serde_json::to_value(&envelope.snapshot).unwrap();
        assert_eq!(
            serialized.get("hostDetection").and_then(serde_json::Value::as_str),
            Some("workspace-development-extension")
        );
        assert_eq!(
            serialized
                .get("integratedTerminals")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn development_host_state_wins_over_a_newer_production_writer() {
        let mut production = snapshot();
        production.extension_mode = Some("production".to_owned());
        production.extension_path = None;
        let production = RuntimeEnvelope {
            updated_at_unix_ms: 200,
            snapshot: production,
        };
        let development = RuntimeEnvelope {
            updated_at_unix_ms: 100,
            snapshot: snapshot(),
        };
        assert!(runtime_envelope_preferred(&development, &production));
        assert!(!runtime_envelope_preferred(&production, &development));
    }

    #[test]
    fn development_path_matching_is_separator_and_case_insensitive_on_windows() {
        #[cfg(windows)]
        assert!(same_development_path(
            r"C:\Work\Tri-Up\",
            "c:/work/tri-up"
        ));

        #[cfg(not(windows))]
        assert!(same_development_path("/work/tri-up/", "/work/tri-up"));
    }

    #[test]
    fn candidate_scan_includes_per_host_sidecars() {
        let directory = env::temp_dir().join(format!(
            "context-capsule-vscode-candidates-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let canonical = directory.join("vscode.json");
        let sidecar = directory.join("vscode-host-123.json");
        let unrelated = directory.join("other.json");
        fs::write(&canonical, b"{}").unwrap();
        fs::write(&sidecar, b"{}").unwrap();
        fs::write(&unrelated, b"{}").unwrap();

        let candidates = runtime_state_candidates(&canonical);
        assert!(candidates.contains(&canonical));
        assert!(candidates.contains(&sidecar));
        assert!(!candidates.contains(&unrelated));
        fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn rejects_wrong_schema() {
        let mut value = snapshot();
        value.schema_version = 99;
        assert!(validate_snapshot(&value).is_err());
    }
}
