use crate::{
    logging::{self, LogLevel},
    persistence::{CapsuleStore, PersistenceError},
    restore_bus::{self, RestoreRequest},
};
use serde::{Deserialize, Serialize};
use std::{
    env,
    error::Error,
    fmt, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const FIREFOX_EXTENSION_ID: &str = "firefox@contextcapsule.app";
pub const NATIVE_HOST_NAME: &str = "com.contextcapsule.host";
pub const NATIVE_PROTOCOL_VERSION: u32 = 1;
pub const BROWSER_SNAPSHOT_SCHEMA_VERSION: u32 = 1;
const MAX_NATIVE_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const LIVE_STATE_MAX_AGE: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirefoxSnapshot {
    pub schema_version: u32,
    pub browser: String,
    pub extension_version: String,
    pub captured_at_unix_ms: i64,
    pub skipped_private_windows: usize,
    pub windows: Vec<BrowserWindowSnapshot>,
}

impl FirefoxSnapshot {
    pub fn tab_count(&self) -> usize {
        self.windows.iter().map(|window| window.tabs.len()).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserWindowSnapshot {
    pub key: String,
    pub focused: bool,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
    pub tabs: Vec<BrowserTabSnapshot>,
    pub groups: Vec<BrowserTabGroupSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserTabSnapshot {
    pub index: i32,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub pinned: bool,
    pub active: bool,
    pub discarded: bool,
    pub muted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cookie_store_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_key: Option<String>,
    pub restorable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserTabGroupSnapshot {
    pub key: String,
    pub title: String,
    pub color: String,
    pub collapsed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    split_orientation: Option<String>,
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

#[derive(Debug)]
pub enum BrowserError {
    Io(io::Error),
    Json(serde_json::Error),
    Persistence(PersistenceError),
    Invalid(String),
    Command(String),
}

impl fmt::Display for BrowserError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Json(error) => write!(formatter, "JSON error: {error}"),
            Self::Persistence(error) => write!(formatter, "{error}"),
            Self::Invalid(message) => write!(formatter, "{message}"),
            Self::Command(message) => write!(formatter, "{message}"),
        }
    }
}

impl Error for BrowserError {}
impl From<io::Error> for BrowserError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<serde_json::Error> for BrowserError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
impl From<PersistenceError> for BrowserError {
    fn from(error: PersistenceError) -> Self {
        Self::Persistence(error)
    }
}

pub fn run_native_host() -> Result<(), BrowserError> {
    logging::info("firefox", "native messaging host session started");
    let stdin = io::stdin();
    let stdout = io::stdout();
    let result = run_native_host_io(stdin.lock(), stdout.lock());
    match &result {
        Ok(()) => logging::info("firefox", "native messaging host session ended"),
        Err(_) => logging::error("firefox", "native messaging host session failed"),
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
    let result = handle_request_inner(request);
    match result {
        Ok(mut response) => {
            response.request_id = request_id;
            response
        }
        Err(error) => {
            logging::error(
                "firefox",
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
            response.restore_request = restore_bus::read_request("firefox")?;
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
            logging::append("firefox", level, message)?;
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
                    "firefox",
                    restore_request_id,
                    restore_error.is_none(),
                    restore_changed,
                    restore_skipped,
                    request.restore_warnings.clone(),
                    restore_error.clone(),
                )?;
            }

            logging::info(
                "firefox",
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
                    logging::error("firefox", message);
                } else if restore_warning_count > 0 {
                    logging::warn("firefox", message);
                } else {
                    logging::info("firefox", message);
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
            let snapshot = firefox_snapshot_from_capsule(name)?;
            logging::info(
                "firefox",
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
        "browser.window.blank.create" => {
            create_blank_browser_window()?;
            logging::info(
                "firefox",
                "native independent blank browser window requested",
            );
            Ok(success_response("browser.window.blank.created"))
        }
        "browser.zen.split.invoke" => {
            let orientation = request
                .split_orientation
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    BrowserError::Invalid(
                        "browser.zen.split.invoke requires split_orientation".to_owned(),
                    )
                })?;
            invoke_zen_split(orientation)?;
            logging::info(
                "firefox",
                format!("native Zen split shortcut invoked; orientation={orientation}"),
            );
            Ok(success_response("browser.zen.split.invoked"))
        }
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
            "unsupported Firefox snapshot schema {}; expected {}",
            snapshot.schema_version, BROWSER_SNAPSHOT_SCHEMA_VERSION
        )));
    }
    if snapshot.browser != "firefox" {
        return Err(BrowserError::Invalid(format!(
            "native host received browser '{}' instead of firefox",
            snapshot.browser
        )));
    }
    if snapshot.windows.len() > 256 {
        return Err(BrowserError::Invalid(
            "Firefox snapshot contains too many windows".to_owned(),
        ));
    }
    if snapshot.tab_count() > 100_000 {
        return Err(BrowserError::Invalid(
            "Firefox snapshot contains too many tabs".to_owned(),
        ));
    }
    Ok(())
}

fn is_zen_executable(name: &str, executable_path: &str) -> bool {
    let executable_name = executable_path
        .rsplit(|character| character == '/' || character == '\\')
        .next()
        .unwrap_or(executable_path);
    (name.eq_ignore_ascii_case("zen") || name.eq_ignore_ascii_case("Zen Browser"))
        && (executable_name.eq_ignore_ascii_case("zen.exe")
            || executable_name.eq_ignore_ascii_case("zen"))
}

#[cfg(windows)]
fn create_blank_browser_window() -> Result<(), BrowserError> {
    let desktop = crate::desktop::discover().map_err(|error| {
        BrowserError::Command(format!("could not inspect running browsers: {error}"))
    })?;
    let executable = desktop
        .applications
        .iter()
        .find_map(|application| {
            let path = application.executable_path.as_deref()?;
            is_zen_executable(&application.name, path).then(|| path.to_owned())
        })
        .ok_or_else(|| {
            BrowserError::Command(
                "Zen blank-window fallback is unavailable because no running zen.exe application was detected"
                    .to_owned(),
            )
        })?;

    Command::new(&executable)
        .arg("--blank-window")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            BrowserError::Command(format!(
                "failed to launch Zen blank window using '{}': {error}",
                executable
            ))
        })?;
    Ok(())
}

#[cfg(not(windows))]
fn create_blank_browser_window() -> Result<(), BrowserError> {
    Err(BrowserError::Command(
        "Zen blank-window native fallback is currently implemented for Windows only".to_owned(),
    ))
}

#[cfg(windows)]
fn invoke_zen_split(orientation: &str) -> Result<(), BrowserError> {
    crate::zen_shortcuts::invoke_split_shortcut(orientation).map_err(BrowserError::Command)
}

#[cfg(not(windows))]
fn invoke_zen_split(_orientation: &str) -> Result<(), BrowserError> {
    Err(BrowserError::Command(
        "Zen split shortcut invocation is currently implemented for Windows only".to_owned(),
    ))
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

pub fn load_recent_firefox_state() -> Result<Option<FirefoxSnapshot>, BrowserError> {
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
    if let Some(path) = env::var_os("CONTEXT_CAPSULE_FIREFOX_STATE_PATH") {
        if path.is_empty() {
            return Err(BrowserError::Invalid(
                "CONTEXT_CAPSULE_FIREFOX_STATE_PATH is empty".to_owned(),
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
            .join("firefox.json"));
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
            .join("firefox.json"));
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        if let Some(base) = env::var_os("XDG_STATE_HOME") {
            return Ok(PathBuf::from(base)
                .join("context-capsule")
                .join("firefox.json"));
        }
        let home = env::var_os("HOME")
            .ok_or_else(|| BrowserError::Invalid("HOME is not available".to_owned()))?;
        Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("context-capsule")
            .join("firefox.json"))
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

fn firefox_snapshot_from_capsule(name: &str) -> Result<FirefoxSnapshot, BrowserError> {
    let store = CapsuleStore::open_default()?;
    let stored = store.load(name)?;
    let value = stored
        .snapshot
        .pointer("/browsers/firefox")
        .cloned()
        .filter(|value| !value.is_null())
        .ok_or_else(|| {
            BrowserError::Invalid(format!("capsule '{name}' has no Firefox snapshot"))
        })?;
    let snapshot: FirefoxSnapshot = serde_json::from_value(value)?;
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

#[derive(Serialize)]
struct NativeManifest<'a> {
    name: &'a str,
    description: &'a str,
    path: String,
    #[serde(rename = "type")]
    kind: &'a str,
    allowed_extensions: [&'a str; 1],
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
    if let Some(path) = env::var_os("CONTEXT_CAPSULE_FIREFOX_MANIFEST_PATH") {
        if path.is_empty() {
            return Err(BrowserError::Invalid(
                "CONTEXT_CAPSULE_FIREFOX_MANIFEST_PATH is empty".to_owned(),
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
            .join("Mozilla")
            .join("NativeMessagingHosts")
            .join(format!("{NATIVE_HOST_NAME}.json")));
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let home = env::var_os("HOME")
            .ok_or_else(|| BrowserError::Invalid("HOME is not available".to_owned()))?;
        Ok(PathBuf::from(home)
            .join(".mozilla")
            .join("native-messaging-hosts")
            .join(format!("{NATIVE_HOST_NAME}.json")))
    }
}

fn native_manifest(executable: &Path) -> NativeManifest<'static> {
    NativeManifest {
        name: NATIVE_HOST_NAME,
        description: "Context Capsule Firefox native messaging host",
        path: executable.to_string_lossy().to_string(),
        kind: "stdio",
        allowed_extensions: [FIREFOX_EXTENSION_ID],
    }
}

#[cfg(windows)]
fn register_windows_manifest(path: &Path) -> Result<(), BrowserError> {
    let key = format!(r"HKCU\Software\Mozilla\NativeMessagingHosts\{NATIVE_HOST_NAME}");
    let output = Command::new("reg.exe")
        .args(["add", &key, "/ve", "/t", "REG_SZ", "/d"])
        .arg(path)
        .arg("/f")
        .output()?;
    if !output.status.success() {
        return Err(BrowserError::Command(format!(
            "failed to register Firefox native host: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn unregister_windows_manifest() -> Result<(), BrowserError> {
    let key = format!(r"HKCU\Software\Mozilla\NativeMessagingHosts\{NATIVE_HOST_NAME}");
    let output = Command::new("reg.exe")
        .args(["delete", &key, "/f"])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.to_ascii_lowercase().contains("unable to find") {
            return Err(BrowserError::Command(format!(
                "failed to unregister Firefox native host: {}",
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
    use std::io::Cursor;

    fn sample_snapshot() -> FirefoxSnapshot {
        FirefoxSnapshot {
            schema_version: 1,
            browser: "firefox".to_owned(),
            extension_version: "0.1.0".to_owned(),
            captured_at_unix_ms: 1,
            skipped_private_windows: 0,
            windows: vec![BrowserWindowSnapshot {
                key: "window-0".to_owned(),
                focused: true,
                state: "normal".to_owned(),
                left: Some(10),
                top: Some(20),
                width: Some(1000),
                height: Some(800),
                tabs: vec![BrowserTabSnapshot {
                    index: 0,
                    url: "https://example.com".to_owned(),
                    title: Some("Example".to_owned()),
                    pinned: false,
                    active: true,
                    discarded: false,
                    muted: false,
                    cookie_store_id: None,
                    group_key: Some("group-0".to_owned()),
                    restorable: true,
                }],
                groups: vec![BrowserTabGroupSnapshot {
                    key: "group-0".to_owned(),
                    title: "Work".to_owned(),
                    color: "blue".to_owned(),
                    collapsed: false,
                }],
            }],
        }
    }

    #[test]
    fn runtime_state_round_trips() {
        let path = env::temp_dir().join(format!(
            "context-capsule-firefox-state-{}-{}.json",
            std::process::id(),
            now_unix_ms()
        ));
        let snapshot = sample_snapshot();
        write_runtime_state_at(&path, &snapshot, 42).expect("write state");
        let loaded = read_runtime_state_at(&path).expect("read state");
        assert_eq!(loaded.updated_at_unix_ms, 42);
        assert_eq!(loaded.snapshot, snapshot);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn native_ping_uses_firefox_framing() {
        let request = serde_json::json!({
            "protocol_version": 1,
            "request_id": "test-request",
            "type": "ping"
        });
        let payload = serde_json::to_vec(&request).unwrap();
        let mut input = Vec::new();
        input.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        input.extend_from_slice(&payload);
        let mut output = Vec::new();
        run_native_host_io(Cursor::new(input), &mut output).expect("host run");

        let length = u32::from_le_bytes(output[..4].try_into().unwrap()) as usize;
        let response: NativeResponse = serde_json::from_slice(&output[4..4 + length]).unwrap();
        assert!(response.ok);
        assert_eq!(response.kind, "pong");
        assert_eq!(response.request_id, "test-request");
    }

    #[test]
    fn validation_rejects_wrong_browser_or_schema() {
        let mut snapshot = sample_snapshot();
        snapshot.browser = "chrome".to_owned();
        assert!(validate_snapshot(&snapshot).is_err());
        snapshot.browser = "firefox".to_owned();
        snapshot.schema_version = 99;
        assert!(validate_snapshot(&snapshot).is_err());
    }

    #[test]
    fn browser_log_levels_are_strict_and_case_insensitive() {
        assert_eq!(parse_native_log_level(None).unwrap(), LogLevel::Info);
        assert_eq!(
            parse_native_log_level(Some("WARN")).unwrap(),
            LogLevel::Warn
        );
        assert_eq!(
            parse_native_log_level(Some("trace")).unwrap(),
            LogLevel::Trace
        );
        assert!(parse_native_log_level(Some("verbose")).is_err());
    }

    #[test]
    fn zen_blank_window_detection_requires_the_expected_application_and_binary() {
        assert!(is_zen_executable(
            "zen",
            r"C:\Program Files\Zen Browser\zen.exe"
        ));
        assert!(is_zen_executable("Zen Browser", "/opt/zen/zen"));
        assert!(!is_zen_executable("zen", r"C:\Windows\System32\cmd.exe"));
        assert!(!is_zen_executable(
            "Firefox",
            r"C:\Program Files\Zen Browser\zen.exe"
        ));
    }

    #[test]
    fn manifest_authorizes_only_context_capsule_extension() {
        let manifest = native_manifest(Path::new("/tmp/capsule-firefox-host"));
        assert_eq!(manifest.name, NATIVE_HOST_NAME);
        assert_eq!(manifest.allowed_extensions, [FIREFOX_EXTENSION_ID]);
        assert_eq!(manifest.kind, "stdio");
    }
}
