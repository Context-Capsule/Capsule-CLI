use crate::{
    browser::{BROWSER_SNAPSHOT_SCHEMA_VERSION, BrowserError, FirefoxSnapshot},
    logging::{self, LogLevel},
    persistence::CapsuleStore,
    restore_bus::{self, RestoreRequest},
};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Sender},
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const CHROME_EXTENSION_ID: &str = "gmffhdppfaeonombpbbgnldagfeabiof";
pub const NATIVE_HOST_NAME: &str = "com.contextcapsule.chrome";
pub const NATIVE_PROTOCOL_VERSION: u32 = 1;
const MAX_NATIVE_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const LIVE_STATE_MAX_AGE: Duration = Duration::from_secs(90);
const NATIVE_SESSION_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const NATIVE_SESSION_MAX_AGE: Duration = Duration::from_secs(15);
const NATIVE_SESSION_PREFIX: &str = "chrome-native-session-";
const ADAPTER: &str = "chrome";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NativeSessionHeartbeat {
    pid: u32,
    started_at_unix_ms: i64,
    updated_at_unix_ms: i64,
}

struct NativeSessionLease {
    path: PathBuf,
    stop: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl NativeSessionLease {
    fn start() -> Result<Self, BrowserError> {
        let pid = std::process::id();
        let path = native_session_path(pid)?;
        let started_at_unix_ms = now_unix_ms();
        write_native_session_heartbeat(&path, pid, started_at_unix_ms, started_at_unix_ms)?;

        let worker_path = path.clone();
        let (stop_tx, stop_rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("context-capsule-chrome-liveness".to_owned())
            .spawn(move || {
                loop {
                    match stop_rx.recv_timeout(NATIVE_SESSION_HEARTBEAT_INTERVAL) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let _ = write_native_session_heartbeat(
                                &worker_path,
                                pid,
                                started_at_unix_ms,
                                now_unix_ms(),
                            );
                        }
                    }
                }
            })
            .map_err(BrowserError::Io)?;

        Ok(Self {
            path,
            stop: Some(stop_tx),
            worker: Some(worker),
        })
    }
}

impl Drop for NativeSessionLease {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct RuntimeStateEnvelope {
    updated_at_unix_ms: i64,
    snapshot: FirefoxSnapshot,
}

#[derive(Debug, Deserialize)]
struct NativeRequest {
    protocol_version: u32,
    request_id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    snapshot: Option<FirefoxSnapshot>,
    #[serde(default)]
    capsule_name: Option<String>,
    #[serde(default)]
    restore_request_id: Option<String>,
    #[serde(default)]
    restore_changed: Option<usize>,
    #[serde(default)]
    restore_skipped: Option<usize>,
    #[serde(default)]
    restore_warnings: Vec<String>,
    #[serde(default)]
    restore_error: Option<String>,
    #[serde(default)]
    log_level: Option<String>,
    #[serde(default)]
    log_message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct NativeResponse {
    protocol_version: u32,
    request_id: String,
    #[serde(rename = "type")]
    kind: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot: Option<FirefoxSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stored_at_unix_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    restore_request: Option<RestoreRequest>,
}

pub fn run_native_host() -> Result<(), BrowserError> {
    logging::info(ADAPTER, "native messaging host session started");
    let lease = if is_native_messaging_invocation() {
        match NativeSessionLease::start() {
            Ok(lease) => Some(lease),
            Err(error) => {
                logging::warn(
                    ADAPTER,
                    format!("native session liveness lease is unavailable: {error}"),
                );
                None
            }
        }
    } else {
        None
    };
    let stdin = io::stdin();
    let stdout = io::stdout();
    let result = run_native_host_io(stdin.lock(), stdout.lock());
    drop(lease);
    match &result {
        Ok(()) => logging::info(ADAPTER, "native messaging host session ended"),
        Err(_) => logging::error(ADAPTER, "native messaging host session failed"),
    }
    result
}

fn run_native_host_io<R: Read, W: Write>(mut reader: R, mut writer: W) -> Result<(), BrowserError> {
    while let Some(payload) = read_native_message(&mut reader)? {
        let request: NativeRequest = serde_json::from_slice(&payload)?;
        let response = handle_request(request);
        write_native_message(&mut writer, &serde_json::to_vec(&response)?)?;
    }
    Ok(())
}

fn handle_request(request: NativeRequest) -> NativeResponse {
    let request_id = request.request_id.clone();
    let request_kind = request.kind.clone();
    match handle_request_inner(request) {
        Ok(mut response) => {
            response.request_id = request_id;
            response
        }
        Err(error) => {
            logging::error(
                ADAPTER,
                format!("native request failed; type={request_kind}; error={error}"),
            );
            NativeResponse {
                protocol_version: NATIVE_PROTOCOL_VERSION,
                request_id,
                kind: "error".to_owned(),
                ok: false,
                error: Some(error.to_string()),
                snapshot: None,
                stored_at_unix_ms: None,
                host_version: None,
                restore_request: None,
            }
        }
    }
}

fn handle_request_inner(request: NativeRequest) -> Result<NativeResponse, BrowserError> {
    if request.protocol_version != NATIVE_PROTOCOL_VERSION {
        return Err(BrowserError::Invalid(format!(
            "unsupported native protocol version {}; expected {}",
            request.protocol_version, NATIVE_PROTOCOL_VERSION
        )));
    }
    if request.request_id.is_empty() || request.request_id.len() > 256 {
        return Err(BrowserError::Invalid("invalid request_id".to_owned()));
    }

    match request.kind.as_str() {
        "ping" => {
            let mut response = success_response("pong");
            response.restore_request = restore_bus::read_request(ADAPTER)?;
            Ok(response)
        }
        "browser.log.append" => {
            let level = parse_native_log_level(request.log_level.as_deref())?;
            let message = request
                .log_message
                .as_deref()
                .map(str::trim)
                .filter(|message| !message.is_empty())
                .ok_or_else(|| {
                    BrowserError::Invalid("browser.log.append requires log_message".to_owned())
                })?;
            logging::append(ADAPTER, level, message)?;
            Ok(success_response("browser.log.appended"))
        }
        "browser.state.update" => {
            let snapshot = request.snapshot.ok_or_else(|| {
                BrowserError::Invalid("browser.state.update requires snapshot".to_owned())
            })?;
            validate_snapshot(&snapshot)?;
            let stored_at = write_runtime_state(&snapshot)?;
            let restore_completion = request.restore_request_id.is_some();
            let restore_changed = request.restore_changed.unwrap_or(0);
            let restore_skipped = request.restore_skipped.unwrap_or(0);
            let restore_warning_count = request.restore_warnings.len();
            let restore_error = request.restore_error.clone();

            if let Some(restore_request_id) = request.restore_request_id.as_deref() {
                restore_bus::complete_request(
                    ADAPTER,
                    restore_request_id,
                    restore_error.is_none(),
                    restore_changed,
                    restore_skipped,
                    request.restore_warnings.clone(),
                    restore_error.clone(),
                )?;
            }

            logging::info(
                ADAPTER,
                format!(
                    "semantic state synchronized; windows={} tabs={} private_skipped={} extension_version={} restore_completion={restore_completion}",
                    snapshot.windows.len(),
                    snapshot.tab_count(),
                    snapshot.skipped_private_windows,
                    snapshot.extension_version,
                ),
            );
            if restore_completion {
                let message = format!(
                    "restore completed; changed={restore_changed} skipped={restore_skipped} warnings={restore_warning_count} ok={}",
                    restore_error.is_none()
                );
                if restore_error.is_some() {
                    logging::error(ADAPTER, message);
                } else if restore_warning_count > 0 {
                    logging::warn(ADAPTER, message);
                } else {
                    logging::info(ADAPTER, message);
                }
            }

            let mut response = success_response("browser.state.updated");
            response.stored_at_unix_ms = Some(stored_at);
            Ok(response)
        }
        "browser.capsule.get" => {
            let name = request
                .capsule_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    BrowserError::Invalid("browser.capsule.get requires capsule_name".to_owned())
                })?;
            let snapshot = chrome_snapshot_from_capsule(name)?;
            logging::info(
                ADAPTER,
                format!(
                    "capsule browser snapshot loaded; windows={} tabs={}",
                    snapshot.windows.len(),
                    snapshot.tab_count()
                ),
            );
            let mut response = success_response("browser.capsule.snapshot");
            response.snapshot = Some(snapshot);
            Ok(response)
        }
        "browser.window.blank.create" => Err(BrowserError::Invalid(
            "Chrome uses the standard extension windows API and has no native blank-window command"
                .to_owned(),
        )),
        "browser.zen.split.invoke" => Err(BrowserError::Invalid(
            "Zen split commands are unavailable through the Chrome native host".to_owned(),
        )),
        other => Err(BrowserError::Invalid(format!(
            "unknown native request type '{other}'"
        ))),
    }
}

fn parse_native_log_level(level: Option<&str>) -> Result<LogLevel, BrowserError> {
    match level.unwrap_or("info").trim().to_ascii_lowercase().as_str() {
        "error" => Ok(LogLevel::Error),
        "warn" | "warning" => Ok(LogLevel::Warn),
        "info" => Ok(LogLevel::Info),
        "debug" => Ok(LogLevel::Debug),
        "trace" => Ok(LogLevel::Trace),
        other => Err(BrowserError::Invalid(format!(
            "unsupported browser log level '{other}'"
        ))),
    }
}

fn success_response(kind: &str) -> NativeResponse {
    NativeResponse {
        protocol_version: NATIVE_PROTOCOL_VERSION,
        request_id: String::new(),
        kind: kind.to_owned(),
        ok: true,
        error: None,
        snapshot: None,
        stored_at_unix_ms: None,
        host_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        restore_request: None,
    }
}

fn validate_snapshot(snapshot: &FirefoxSnapshot) -> Result<(), BrowserError> {
    if snapshot.schema_version != BROWSER_SNAPSHOT_SCHEMA_VERSION {
        return Err(BrowserError::Invalid(format!(
            "unsupported Chrome snapshot schema {}; expected {}",
            snapshot.schema_version, BROWSER_SNAPSHOT_SCHEMA_VERSION
        )));
    }
    if snapshot.browser != ADAPTER {
        return Err(BrowserError::Invalid(format!(
            "Chrome native host received browser '{}' instead of chrome",
            snapshot.browser
        )));
    }
    if snapshot.windows.len() > 256 {
        return Err(BrowserError::Invalid(
            "Chrome snapshot contains too many windows".to_owned(),
        ));
    }
    if snapshot.tab_count() > 100_000 {
        return Err(BrowserError::Invalid(
            "Chrome snapshot contains too many tabs".to_owned(),
        ));
    }
    Ok(())
}

fn read_native_message<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>, BrowserError> {
    let mut length_bytes = [0_u8; 4];
    let read = reader.read(&mut length_bytes[..1])?;
    if read == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut length_bytes[1..])?;
    let length = u32::from_le_bytes(length_bytes) as usize;
    if length == 0 || length > MAX_NATIVE_MESSAGE_BYTES {
        return Err(BrowserError::Invalid(format!(
            "invalid native message length {length}"
        )));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    Ok(Some(payload))
}

fn write_native_message<W: Write>(writer: &mut W, payload: &[u8]) -> Result<(), BrowserError> {
    if payload.is_empty() || payload.len() > MAX_NATIVE_MESSAGE_BYTES {
        return Err(BrowserError::Invalid(format!(
            "invalid native response length {}",
            payload.len()
        )));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| BrowserError::Invalid("native response is too large".to_owned()))?;
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

pub fn extension_connected() -> Result<bool, BrowserError> {
    let state_path = runtime_state_path()?;
    Ok(newest_live_native_session_for_state(&state_path, now_unix_ms())?.is_some())
}

fn is_native_messaging_invocation() -> bool {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    is_native_messaging_arguments(&arguments)
}

fn is_native_messaging_arguments(arguments: &[String]) -> bool {
    let expected_origin = format!("chrome-extension://{CHROME_EXTENSION_ID}/");
    arguments
        .first()
        .is_some_and(|origin| origin.eq_ignore_ascii_case(&expected_origin))
}

fn native_session_path(pid: u32) -> Result<PathBuf, BrowserError> {
    let state_path = runtime_state_path()?;
    let directory = state_path.parent().ok_or_else(|| {
        BrowserError::Invalid("Chrome runtime state path has no parent directory".to_owned())
    })?;
    Ok(directory.join(format!("{NATIVE_SESSION_PREFIX}{pid}.json")))
}

fn write_native_session_heartbeat(
    path: &Path,
    pid: u32,
    started_at_unix_ms: i64,
    updated_at_unix_ms: i64,
) -> Result<(), BrowserError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let heartbeat = NativeSessionHeartbeat {
        pid,
        started_at_unix_ms,
        updated_at_unix_ms,
    };
    let bytes = serde_json::to_vec(&heartbeat)?;
    let temporary = path.with_extension(format!("json.{pid}.tmp"));
    fs::write(&temporary, bytes)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}

fn newest_live_native_session_for_state(
    state_path: &Path,
    now_ms: i64,
) -> Result<Option<i64>, BrowserError> {
    let Some(directory) = state_path.parent() else {
        return Ok(None);
    };
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(BrowserError::Io(error)),
    };

    let max_age_ms = NATIVE_SESSION_MAX_AGE.as_millis() as i64;
    let mut newest_started_at = None::<i64>;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(NATIVE_SESSION_PREFIX) || !name.ends_with(".json") {
            continue;
        }
        let heartbeat = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<NativeSessionHeartbeat>(&bytes).ok());
        let Some(heartbeat) = heartbeat else {
            let _ = fs::remove_file(path);
            continue;
        };
        let age_ms = now_ms.saturating_sub(heartbeat.updated_at_unix_ms);
        if age_ms <= max_age_ms {
            newest_started_at = Some(
                newest_started_at
                    .map(|value| value.max(heartbeat.started_at_unix_ms))
                    .unwrap_or(heartbeat.started_at_unix_ms),
            );
        } else {
            let _ = fs::remove_file(path);
        }
    }
    Ok(newest_started_at)
}

pub fn load_recent_chrome_state() -> Result<Option<FirefoxSnapshot>, BrowserError> {
    let path = runtime_state_path()?;
    let envelope = match read_runtime_state_at(&path) {
        Ok(envelope) => envelope,
        Err(BrowserError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let age_ms = now_unix_ms().saturating_sub(envelope.updated_at_unix_ms);
    if age_ms > LIVE_STATE_MAX_AGE.as_millis() as i64 {
        return Ok(None);
    }
    validate_snapshot(&envelope.snapshot)?;
    Ok(Some(envelope.snapshot))
}

pub fn runtime_state_path() -> Result<PathBuf, BrowserError> {
    if let Some(path) = env::var_os("CONTEXT_CAPSULE_CHROME_STATE_PATH") {
        if path.is_empty() {
            return Err(BrowserError::Invalid(
                "CONTEXT_CAPSULE_CHROME_STATE_PATH is empty".to_owned(),
            ));
        }
        return Ok(PathBuf::from(path));
    }

    #[cfg(windows)]
    {
        let base = env::var_os("LOCALAPPDATA")
            .ok_or_else(|| BrowserError::Invalid("LOCALAPPDATA is not available".to_owned()))?;
        return Ok(PathBuf::from(base)
            .join("ContextCapsule")
            .join("runtime")
            .join("chrome.json"));
    }

    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME")
            .ok_or_else(|| BrowserError::Invalid("HOME is not available".to_owned()))?;
        return Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("ContextCapsule")
            .join("runtime")
            .join("chrome.json"));
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        if let Some(base) = env::var_os("XDG_STATE_HOME") {
            return Ok(PathBuf::from(base)
                .join("context-capsule")
                .join("chrome.json"));
        }
        let home = env::var_os("HOME")
            .ok_or_else(|| BrowserError::Invalid("HOME is not available".to_owned()))?;
        Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("context-capsule")
            .join("chrome.json"))
    }
}

fn write_runtime_state(snapshot: &FirefoxSnapshot) -> Result<i64, BrowserError> {
    let path = runtime_state_path()?;
    let updated_at = now_unix_ms();
    write_runtime_state_at(&path, snapshot, updated_at)?;
    Ok(updated_at)
}

fn write_runtime_state_at(
    path: &Path,
    snapshot: &FirefoxSnapshot,
    updated_at: i64,
) -> Result<(), BrowserError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let envelope = RuntimeStateEnvelope {
        updated_at_unix_ms: updated_at,
        snapshot: snapshot.clone(),
    };
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&temporary, serde_json::to_vec(&envelope)?)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}

fn read_runtime_state_at(path: &Path) -> Result<RuntimeStateEnvelope, BrowserError> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn chrome_snapshot_from_capsule(name: &str) -> Result<FirefoxSnapshot, BrowserError> {
    let store = CapsuleStore::open_default()?;
    let stored = store.load(name)?;
    let value = stored
        .snapshot
        .pointer("/browsers/chrome")
        .cloned()
        .filter(|value| !value.is_null())
        .ok_or_else(|| BrowserError::Invalid(format!("capsule '{name}' has no Chrome snapshot")))?;
    let snapshot: FirefoxSnapshot = serde_json::from_value(value)?;
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

#[derive(Serialize)]
struct NativeManifest {
    name: &'static str,
    description: &'static str,
    path: String,
    #[serde(rename = "type")]
    kind: &'static str,
    allowed_origins: [String; 1],
}

pub fn install_native_host() -> Result<PathBuf, BrowserError> {
    let executable = env::current_exe()?;
    let manifest_path = native_manifest_path()?;
    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let manifest = native_manifest(&executable);
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

    #[cfg(windows)]
    register_windows_manifest(&manifest_path)?;

    Ok(manifest_path)
}

pub fn uninstall_native_host() -> Result<PathBuf, BrowserError> {
    let manifest_path = native_manifest_path()?;
    #[cfg(windows)]
    unregister_windows_manifest()?;
    match fs::remove_file(&manifest_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(manifest_path)
}

pub fn native_manifest_path() -> Result<PathBuf, BrowserError> {
    if let Some(path) = env::var_os("CONTEXT_CAPSULE_CHROME_MANIFEST_PATH") {
        if path.is_empty() {
            return Err(BrowserError::Invalid(
                "CONTEXT_CAPSULE_CHROME_MANIFEST_PATH is empty".to_owned(),
            ));
        }
        return Ok(PathBuf::from(path));
    }

    #[cfg(windows)]
    {
        let base = env::var_os("LOCALAPPDATA")
            .ok_or_else(|| BrowserError::Invalid("LOCALAPPDATA is not available".to_owned()))?;
        return Ok(PathBuf::from(base)
            .join("ContextCapsule")
            .join("native-messaging")
            .join(format!("{NATIVE_HOST_NAME}.json")));
    }

    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME")
            .ok_or_else(|| BrowserError::Invalid("HOME is not available".to_owned()))?;
        return Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Google")
            .join("Chrome")
            .join("NativeMessagingHosts")
            .join(format!("{NATIVE_HOST_NAME}.json")));
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let home = env::var_os("HOME")
            .ok_or_else(|| BrowserError::Invalid("HOME is not available".to_owned()))?;
        Ok(PathBuf::from(home)
            .join(".config")
            .join("google-chrome")
            .join("NativeMessagingHosts")
            .join(format!("{NATIVE_HOST_NAME}.json")))
    }
}

fn native_manifest(executable: &Path) -> NativeManifest {
    NativeManifest {
        name: NATIVE_HOST_NAME,
        description: "Context Capsule Chrome native messaging host",
        path: executable.to_string_lossy().to_string(),
        kind: "stdio",
        allowed_origins: [format!("chrome-extension://{CHROME_EXTENSION_ID}/")],
    }
}

#[cfg(windows)]
fn register_windows_manifest(path: &Path) -> Result<(), BrowserError> {
    let key = format!(r"HKCU\Software\Google\Chrome\NativeMessagingHosts\{NATIVE_HOST_NAME}");
    let output = Command::new("reg.exe")
        .args(["add", &key, "/ve", "/t", "REG_SZ", "/d"])
        .arg(path)
        .arg("/f")
        .output()?;
    if !output.status.success() {
        return Err(BrowserError::Command(format!(
            "failed to register Chrome native host: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn unregister_windows_manifest() -> Result<(), BrowserError> {
    let key = format!(r"HKCU\Software\Google\Chrome\NativeMessagingHosts\{NATIVE_HOST_NAME}");
    let output = Command::new("reg.exe")
        .args(["delete", &key, "/f"])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.to_ascii_lowercase().contains("unable to find") {
            return Err(BrowserError::Command(format!(
                "failed to unregister Chrome native host: {}",
                stderr.trim()
            )));
        }
    }
    Ok(())
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chrome_native_arguments_require_our_extension_origin() {
        assert!(is_native_messaging_arguments(&[format!(
            "chrome-extension://{CHROME_EXTENSION_ID}/"
        )]));
        assert!(!is_native_messaging_arguments(&[]));
        assert!(!is_native_messaging_arguments(&[
            "chrome-extension://wrong-extension/".to_owned(),
        ]));
    }

    use crate::browser::{BrowserTabSnapshot, BrowserWindowSnapshot};
    use std::io::Cursor;

    fn sample_snapshot() -> FirefoxSnapshot {
        FirefoxSnapshot {
            schema_version: 1,
            browser: "chrome".to_owned(),
            extension_version: "0.1.7".to_owned(),
            captured_at_unix_ms: 123,
            skipped_private_windows: 0,
            windows: vec![BrowserWindowSnapshot {
                key: "window-0".to_owned(),
                focused: true,
                state: "normal".to_owned(),
                left: None,
                top: None,
                width: None,
                height: None,
                tabs: vec![BrowserTabSnapshot {
                    index: 0,
                    url: "https://example.com".to_owned(),
                    title: None,
                    pinned: false,
                    active: true,
                    discarded: false,
                    muted: false,
                    cookie_store_id: None,
                    group_key: None,
                    restorable: true,
                }],
                groups: Vec::new(),
            }],
        }
    }

    #[test]
    fn validates_chrome_snapshot_and_rejects_firefox_discriminator() {
        assert!(validate_snapshot(&sample_snapshot()).is_ok());
        let mut firefox = sample_snapshot();
        firefox.browser = "firefox".to_owned();
        assert!(validate_snapshot(&firefox).is_err());
    }

    #[test]
    fn chrome_manifest_has_stable_allowed_origin() {
        let manifest = native_manifest(Path::new(r"C:\ContextCapsule\capsule-chrome-host.exe"));
        assert_eq!(manifest.name, NATIVE_HOST_NAME);
        assert_eq!(
            manifest.allowed_origins[0],
            format!("chrome-extension://{CHROME_EXTENSION_ID}/")
        );
    }

    #[test]
    fn native_ping_reads_and_writes_framed_messages() {
        let request = serde_json::to_vec(&serde_json::json!({
            "protocol_version": 1,
            "request_id": "test",
            "type": "ping"
        }))
        .unwrap();
        let mut framed = Vec::new();
        framed.extend_from_slice(&(request.len() as u32).to_le_bytes());
        framed.extend_from_slice(&request);
        let payload = read_native_message(&mut Cursor::new(framed))
            .unwrap()
            .expect("request");
        assert_eq!(payload, request);

        let mut output = Vec::new();
        write_native_message(&mut output, b"{}").unwrap();
        assert_eq!(&output[..4], &2_u32.to_le_bytes());
        assert_eq!(&output[4..], b"{}");
    }
}
