use serde::{Deserialize, Serialize};
use std::{
    env,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const RESTORE_BRIDGE_SCHEMA_VERSION: u32 = 1;
const REQUEST_POLL_INTERVAL: Duration = Duration::from_millis(100);
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreAdapter {
    Firefox,
    VsCode,
}

impl RestoreAdapter {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Firefox => "firefox",
            Self::VsCode => "vscode",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SemanticRestoreRequest {
    pub schema_version: u32,
    pub request_id: String,
    pub adapter: String,
    pub capsule_name: String,
    pub created_at_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SemanticRestoreResult {
    pub schema_version: u32,
    pub request_id: String,
    pub adapter: String,
    pub ok: bool,
    pub completed_at_unix_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RestoreTicket {
    pub adapter: RestoreAdapter,
    pub request_id: String,
    pending_path: PathBuf,
    processing_path: PathBuf,
    result_path: PathBuf,
}

impl RestoreTicket {
    pub fn state(&self) -> Result<RestoreTicketState, RestoreBridgeError> {
        if self.result_path.is_file() {
            return Ok(RestoreTicketState::Completed);
        }
        if self.processing_path.is_file() {
            return Ok(RestoreTicketState::Claimed);
        }
        if self.pending_path.is_file() {
            return Ok(RestoreTicketState::Pending);
        }
        Ok(RestoreTicketState::Missing)
    }

    pub fn read_result(&self) -> Result<Option<SemanticRestoreResult>, RestoreBridgeError> {
        if !self.result_path.is_file() {
            return Ok(None);
        }
        let result = serde_json::from_slice::<SemanticRestoreResult>(&fs::read(&self.result_path)?)?;
        validate_result(&result, self.adapter, &self.request_id)?;
        Ok(Some(result))
    }

    pub fn cancel_pending(&self) -> Result<bool, RestoreBridgeError> {
        match fs::remove_file(&self.pending_path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub fn cleanup(&self) {
        let _ = fs::remove_file(&self.pending_path);
        let _ = fs::remove_file(&self.processing_path);
        let _ = fs::remove_file(&self.result_path);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreTicketState {
    Pending,
    Claimed,
    Completed,
    Missing,
}

#[derive(Debug, Clone)]
pub struct ClaimedRestoreRequest {
    pub request: SemanticRestoreRequest,
    processing_path: PathBuf,
}

#[derive(Debug)]
pub enum RestoreBridgeError {
    Io(io::Error),
    Json(serde_json::Error),
    Invalid(String),
}

impl fmt::Display for RestoreBridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "restore bridge I/O error: {error}"),
            Self::Json(error) => write!(formatter, "restore bridge JSON error: {error}"),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl Error for RestoreBridgeError {}
impl From<io::Error> for RestoreBridgeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<serde_json::Error> for RestoreBridgeError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub fn queue_restore(
    adapter: RestoreAdapter,
    capsule_name: &str,
) -> Result<RestoreTicket, RestoreBridgeError> {
    let capsule_name = capsule_name.trim();
    if capsule_name.is_empty() {
        return Err(RestoreBridgeError::Invalid(
            "restore bridge capsule name cannot be empty".to_owned(),
        ));
    }

    let request_id = next_request_id();
    let request = SemanticRestoreRequest {
        schema_version: RESTORE_BRIDGE_SCHEMA_VERSION,
        request_id: request_id.clone(),
        adapter: adapter.as_str().to_owned(),
        capsule_name: capsule_name.to_owned(),
        created_at_unix_ms: now_unix_ms(),
    };
    let paths = ticket_paths(adapter, &request_id)?;
    if let Some(parent) = paths.pending_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = paths.result_path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write_json(&paths.pending_path, &request)?;
    Ok(RestoreTicket {
        adapter,
        request_id,
        pending_path: paths.pending_path,
        processing_path: paths.processing_path,
        result_path: paths.result_path,
    })
}

pub fn wait_and_claim(
    adapter: RestoreAdapter,
    timeout: Duration,
) -> Result<Option<ClaimedRestoreRequest>, RestoreBridgeError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(request) = claim_next(adapter)? {
            return Ok(Some(request));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(REQUEST_POLL_INTERVAL);
    }
}

pub fn claim_next(
    adapter: RestoreAdapter,
) -> Result<Option<ClaimedRestoreRequest>, RestoreBridgeError> {
    let directory = requests_directory(adapter)?;
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    let mut candidates = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    candidates.sort();

    for pending_path in candidates {
        let Some(file_name) = pending_path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let request_id = file_name.trim_end_matches(".json");
        if request_id.is_empty() {
            continue;
        }
        let processing_path = pending_path.with_extension("processing");
        match fs::rename(&pending_path, &processing_path) {
            Ok(()) => {
                let request = match serde_json::from_slice::<SemanticRestoreRequest>(&fs::read(&processing_path)?) {
                    Ok(request) => request,
                    Err(error) => {
                        let _ = fs::remove_file(&processing_path);
                        return Err(error.into());
                    }
                };
                validate_request(&request, adapter, request_id)?;
                return Ok(Some(ClaimedRestoreRequest {
                    request,
                    processing_path,
                }));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
    }

    Ok(None)
}

pub fn complete_claimed(
    adapter: RestoreAdapter,
    restore_request_id: &str,
    ok: bool,
    summary: Option<String>,
    error: Option<String>,
) -> Result<SemanticRestoreResult, RestoreBridgeError> {
    validate_request_id(restore_request_id)?;
    let paths = ticket_paths(adapter, restore_request_id)?;
    let processing_path = paths.processing_path;
    if !processing_path.is_file() {
        return Err(RestoreBridgeError::Invalid(format!(
            "restore request '{restore_request_id}' is not currently claimed"
        )));
    }

    let result = SemanticRestoreResult {
        schema_version: RESTORE_BRIDGE_SCHEMA_VERSION,
        request_id: restore_request_id.to_owned(),
        adapter: adapter.as_str().to_owned(),
        ok,
        completed_at_unix_ms: now_unix_ms(),
        summary,
        error,
    };
    if let Some(parent) = paths.result_path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write_json(&paths.result_path, &result)?;
    let _ = fs::remove_file(processing_path);
    Ok(result)
}

pub fn release_claim(claim: ClaimedRestoreRequest) -> Result<(), RestoreBridgeError> {
    let pending_path = claim.processing_path.with_extension("json");
    fs::rename(claim.processing_path, pending_path)?;
    Ok(())
}

pub fn restore_root() -> Result<PathBuf, RestoreBridgeError> {
    if let Some(path) = env::var_os("CONTEXT_CAPSULE_RESTORE_DIR") {
        if path.is_empty() {
            return Err(RestoreBridgeError::Invalid(
                "CONTEXT_CAPSULE_RESTORE_DIR is empty".to_owned(),
            ));
        }
        return Ok(PathBuf::from(path));
    }

    #[cfg(windows)]
    {
        let base = env::var_os("LOCALAPPDATA").ok_or_else(|| {
            RestoreBridgeError::Invalid("LOCALAPPDATA is not available".to_owned())
        })?;
        return Ok(PathBuf::from(base).join("ContextCapsule").join("restore"));
    }

    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME")
            .ok_or_else(|| RestoreBridgeError::Invalid("HOME is not available".to_owned()))?;
        return Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("ContextCapsule")
            .join("restore"));
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        if let Some(base) = env::var_os("XDG_STATE_HOME") {
            return Ok(PathBuf::from(base).join("context-capsule").join("restore"));
        }
        let home = env::var_os("HOME")
            .ok_or_else(|| RestoreBridgeError::Invalid("HOME is not available".to_owned()))?;
        Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("context-capsule")
            .join("restore"))
    }
}

fn requests_directory(adapter: RestoreAdapter) -> Result<PathBuf, RestoreBridgeError> {
    Ok(restore_root()?.join("requests").join(adapter.as_str()))
}

fn result_path(request_id: &str) -> Result<PathBuf, RestoreBridgeError> {
    Ok(restore_root()?.join("results").join(format!("{request_id}.json")))
}

struct TicketPaths {
    pending_path: PathBuf,
    processing_path: PathBuf,
    result_path: PathBuf,
}

fn ticket_paths(adapter: RestoreAdapter, request_id: &str) -> Result<TicketPaths, RestoreBridgeError> {
    validate_request_id(request_id)?;
    let directory = requests_directory(adapter)?;
    Ok(TicketPaths {
        pending_path: directory.join(format!("{request_id}.json")),
        processing_path: directory.join(format!("{request_id}.processing")),
        result_path: result_path(request_id)?,
    })
}

fn validate_request(
    request: &SemanticRestoreRequest,
    adapter: RestoreAdapter,
    expected_request_id: &str,
) -> Result<(), RestoreBridgeError> {
    if request.schema_version != RESTORE_BRIDGE_SCHEMA_VERSION {
        return Err(RestoreBridgeError::Invalid(format!(
            "unsupported restore bridge schema {}; expected {}",
            request.schema_version, RESTORE_BRIDGE_SCHEMA_VERSION
        )));
    }
    if request.adapter != adapter.as_str() {
        return Err(RestoreBridgeError::Invalid(format!(
            "restore request targets '{}' instead of '{}'",
            request.adapter,
            adapter.as_str()
        )));
    }
    if request.request_id != expected_request_id {
        return Err(RestoreBridgeError::Invalid(
            "restore request id does not match its file name".to_owned(),
        ));
    }
    if request.capsule_name.trim().is_empty() {
        return Err(RestoreBridgeError::Invalid(
            "restore request has an empty capsule name".to_owned(),
        ));
    }
    Ok(())
}

fn validate_result(
    result: &SemanticRestoreResult,
    adapter: RestoreAdapter,
    expected_request_id: &str,
) -> Result<(), RestoreBridgeError> {
    if result.schema_version != RESTORE_BRIDGE_SCHEMA_VERSION
        || result.adapter != adapter.as_str()
        || result.request_id != expected_request_id
    {
        return Err(RestoreBridgeError::Invalid(
            "restore result does not match the queued request".to_owned(),
        ));
    }
    Ok(())
}

fn validate_request_id(request_id: &str) -> Result<(), RestoreBridgeError> {
    if request_id.is_empty()
        || request_id.len() > 128
        || !request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(RestoreBridgeError::Invalid(
            "invalid restore request id".to_owned(),
        ));
    }
    Ok(())
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), RestoreBridgeError> {
    let parent = path.parent().ok_or_else(|| {
        RestoreBridgeError::Invalid("restore bridge path has no parent directory".to_owned())
    })?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|value| value.to_str()).unwrap_or("restore"),
        std::process::id()
    ));
    fs::write(&temporary, serde_json::to_vec(value)?)?;
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error.into())
        }
    }
}

fn next_request_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{now:x}-{counter:x}", std::process::id())
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

    fn temp_root() -> PathBuf {
        env::temp_dir().join(format!(
            "context-capsule-restore-bridge-{}-{}",
            std::process::id(),
            REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn queue_claim_complete_round_trip() {
        let root = temp_root();
        unsafe { env::set_var("CONTEXT_CAPSULE_RESTORE_DIR", &root) };
        let ticket = queue_restore(RestoreAdapter::Firefox, "demo").unwrap();
        assert_eq!(ticket.state().unwrap(), RestoreTicketState::Pending);

        let claim = claim_next(RestoreAdapter::Firefox).unwrap().unwrap();
        assert_eq!(claim.request.capsule_name, "demo");
        assert_eq!(ticket.state().unwrap(), RestoreTicketState::Claimed);

        complete_claimed(
            RestoreAdapter::Firefox,
            &claim.request.request_id,
            true,
            Some("done".to_owned()),
            None,
        )
        .unwrap();
        assert_eq!(ticket.state().unwrap(), RestoreTicketState::Completed);
        let result = ticket.read_result().unwrap().unwrap();
        assert!(result.ok);
        assert_eq!(result.summary.as_deref(), Some("done"));
        ticket.cleanup();
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn release_returns_claim_to_pending_queue() {
        let root = temp_root();
        unsafe { env::set_var("CONTEXT_CAPSULE_RESTORE_DIR", &root) };
        let ticket = queue_restore(RestoreAdapter::VsCode, "demo").unwrap();
        let claim = claim_next(RestoreAdapter::VsCode).unwrap().unwrap();
        release_claim(claim).unwrap();
        assert_eq!(ticket.state().unwrap(), RestoreTicketState::Pending);
        ticket.cleanup();
        fs::remove_dir_all(root).ok();
    }
}
