use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const RESTORE_BUS_SCHEMA_VERSION: u32 = 1;
const MAX_REQUEST_AGE_MS: i64 = 60_000;
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestoreRequest {
    pub schema_version: u32,
    pub request_id: String,
    pub adapter: String,
    pub created_at_unix_ms: i64,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreCompletion {
    pub schema_version: u32,
    pub request_id: String,
    pub adapter: String,
    pub completed_at_unix_ms: i64,
    pub ok: bool,
    pub changed: usize,
    pub skipped: usize,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn write_request(adapter: &str, payload: Value) -> io::Result<RestoreRequest> {
    validate_adapter(adapter)?;
    let request = RestoreRequest {
        schema_version: RESTORE_BUS_SCHEMA_VERSION,
        request_id: next_request_id(adapter),
        adapter: adapter.to_owned(),
        created_at_unix_ms: now_unix_ms(),
        payload,
    };

    let result = completion_path(adapter)?;
    if result.is_file() {
        let _ = fs::remove_file(result);
    }
    atomic_write_json(&request_path(adapter)?, &request)?;
    Ok(request)
}

pub fn read_request(adapter: &str) -> io::Result<Option<RestoreRequest>> {
    validate_adapter(adapter)?;
    let path = request_path(adapter)?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let request: RestoreRequest = serde_json::from_slice(&bytes).map_err(invalid_json)?;
    if request.schema_version != RESTORE_BUS_SCHEMA_VERSION || request.adapter != adapter {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid {adapter} restore request envelope"),
        ));
    }
    if now_unix_ms().saturating_sub(request.created_at_unix_ms) > MAX_REQUEST_AGE_MS {
        let _ = fs::remove_file(path);
        return Ok(None);
    }
    Ok(Some(request))
}

pub fn cancel_request(adapter: &str, request_id: &str) -> io::Result<bool> {
    validate_adapter(adapter)?;
    let path = request_path(adapter)?;
    let Some(request) = read_request(adapter)? else {
        return Ok(false);
    };
    if request.request_id != request_id {
        return Ok(false);
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn complete_request(
    adapter: &str,
    request_id: &str,
    ok: bool,
    changed: usize,
    skipped: usize,
    warnings: Vec<String>,
    error: Option<String>,
) -> io::Result<RestoreCompletion> {
    validate_adapter(adapter)?;
    let completion = RestoreCompletion {
        schema_version: RESTORE_BUS_SCHEMA_VERSION,
        request_id: request_id.to_owned(),
        adapter: adapter.to_owned(),
        completed_at_unix_ms: now_unix_ms(),
        ok,
        changed,
        skipped,
        warnings,
        error,
    };
    atomic_write_json(&completion_path(adapter)?, &completion)?;

    if let Ok(Some(request)) = read_request(adapter) {
        if request.request_id == request_id {
            let _ = fs::remove_file(request_path(adapter)?);
        }
    }
    Ok(completion)
}

pub fn wait_for_completion(
    adapter: &str,
    request_id: &str,
    timeout: Duration,
) -> io::Result<Option<RestoreCompletion>> {
    validate_adapter(adapter)?;
    let path = completion_path(adapter)?;
    let deadline = Instant::now() + timeout;
    loop {
        match fs::read(&path) {
            Ok(bytes) => {
                let completion: RestoreCompletion =
                    serde_json::from_slice(&bytes).map_err(invalid_json)?;
                if completion.schema_version == RESTORE_BUS_SCHEMA_VERSION
                    && completion.adapter == adapter
                    && completion.request_id == request_id
                {
                    return Ok(Some(completion));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

pub fn request_path(adapter: &str) -> io::Result<PathBuf> {
    validate_adapter(adapter)?;
    Ok(runtime_dir()?.join(format!("{adapter}-request.json")))
}

pub fn completion_path(adapter: &str) -> io::Result<PathBuf> {
    validate_adapter(adapter)?;
    Ok(runtime_dir()?.join(format!("{adapter}-result.json")))
}

pub fn runtime_dir() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("CONTEXT_CAPSULE_RESTORE_RUNTIME_DIR") {
        if path.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CONTEXT_CAPSULE_RESTORE_RUNTIME_DIR is empty",
            ));
        }
        return Ok(PathBuf::from(path));
    }

    #[cfg(windows)]
    {
        let base = env::var_os("LOCALAPPDATA")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is unavailable"))?;
        return Ok(PathBuf::from(base)
            .join("ContextCapsule")
            .join("runtime")
            .join("restore"));
    }

    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unavailable"))?;
        return Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("ContextCapsule")
            .join("runtime")
            .join("restore"));
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        if let Some(base) = env::var_os("XDG_STATE_HOME") {
            return Ok(PathBuf::from(base).join("context-capsule").join("restore"));
        }
        let home = env::var_os("HOME")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unavailable"))?;
        Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("context-capsule")
            .join("restore"))
    }
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec(value).map_err(invalid_json)?;
    fs::write(&temporary, bytes)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary, path)
}

fn validate_adapter(adapter: &str) -> io::Result<()> {
    if matches!(adapter, "firefox" | "vscode") {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported restore adapter '{adapter}'"),
        ))
    }
}

fn invalid_json(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn next_request_id(adapter: &str) -> String {
    let sequence = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{adapter}-{}-{}-{sequence}", std::process::id(), now_unix_ms())
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
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_dir() -> PathBuf {
        env::temp_dir().join(format!(
            "context-capsule-restore-bus-{}-{}",
            std::process::id(),
            now_unix_ms()
        ))
    }

    fn with_runtime_dir(test: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().expect("restore bus environment lock");
        let dir = temp_dir();
        unsafe { env::set_var("CONTEXT_CAPSULE_RESTORE_RUNTIME_DIR", &dir) };
        test();
        fs::remove_dir_all(dir).ok();
        unsafe { env::remove_var("CONTEXT_CAPSULE_RESTORE_RUNTIME_DIR") };
    }

    #[test]
    fn request_and_completion_round_trip() {
        with_runtime_dir(|| {
            let request =
                write_request("firefox", serde_json::json!({"schema_version": 1})).unwrap();
            assert_eq!(read_request("firefox").unwrap().unwrap(), request);
            complete_request(
                "firefox",
                &request.request_id,
                true,
                3,
                1,
                vec!["minor".to_owned()],
                None,
            )
            .unwrap();
            let completion = wait_for_completion(
                "firefox",
                &request.request_id,
                Duration::from_millis(20),
            )
            .unwrap()
            .unwrap();
            assert!(completion.ok);
            assert_eq!(completion.changed, 3);
            assert!(!request_path("firefox").unwrap().exists());
        });
    }

    #[test]
    fn cancellation_only_removes_matching_request() {
        with_runtime_dir(|| {
            let request = write_request("vscode", serde_json::json!({})).unwrap();
            assert!(!cancel_request("vscode", "other").unwrap());
            assert!(request_path("vscode").unwrap().is_file());
            assert!(cancel_request("vscode", &request.request_id).unwrap());
            assert!(!request_path("vscode").unwrap().exists());
        });
    }
}