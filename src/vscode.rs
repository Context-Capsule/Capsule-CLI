use serde::{Deserialize, Serialize};
use std::{
    env,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const VSCODE_SNAPSHOT_SCHEMA_VERSION: u32 = 1;
const LIVE_STATE_MAX_AGE: Duration = Duration::from_secs(90);
const MAX_TABS: usize = 100_000;
const MAX_EXTENSION_PATH_LENGTH: usize = 32_768;
const VSCODE_HOST_STATE_PREFIX: &str = "vscode-host-";
const VSCODE_HOST_STATE_SUFFIX: &str = ".json";

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
#[serde(rename_all = "camelCase")]
pub struct VsCodeSnapshot {
    pub schema_version: u32,
    pub captured_at_unix_ms: i64,
    pub app_name: String,
    pub app_host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension_path: Option<String>,
    pub workspace_trusted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_file: Option<String>,
    pub workspace_folders: Vec<WorkspaceFolderSnapshot>,
    pub tab_groups: Vec<TabGroupSnapshot>,
    pub visible_editor_selections: Vec<EditorSelectionSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_editor_uri: Option<String>,
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
            app_name: "Visual Studio Code".to_owned(),
            app_host: "desktop".to_owned(),
            remote_name: Some("wsl".to_owned()),
            extension_mode: Some("development".to_owned()),
            extension_path: Some(r"C:\work\Capsule-VSCode-Extension".to_owned()),
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
        assert_eq!(loaded.snapshot.tab_count(), 1);
        assert!(loaded.snapshot.is_extension_development_host());
        fs::remove_file(path).ok();
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
