#[path = "browser.rs"]
mod base;

pub use base::{
    BROWSER_SNAPSHOT_SCHEMA_VERSION, BrowserError, BrowserTabGroupSnapshot, BrowserTabSnapshot,
    BrowserWindowSnapshot, FIREFOX_EXTENSION_ID, FirefoxSnapshot, NATIVE_HOST_NAME,
    NATIVE_PROTOCOL_VERSION, install_native_host, native_manifest_path, runtime_state_path,
    uninstall_native_host,
};

use crate::logging;
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io,
    path::{Path, PathBuf},
    sync::mpsc::{self, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const NATIVE_SESSION_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const NATIVE_SESSION_MAX_AGE: Duration = Duration::from_secs(15);
const NATIVE_SESSION_SYNC_GRACE: Duration = Duration::from_secs(2);
const NATIVE_SESSION_SYNC_POLL: Duration = Duration::from_millis(50);
const NATIVE_SESSION_PREFIX: &str = "firefox-native-session-";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NativeSessionHeartbeat {
    pid: u32,
    updated_at_unix_ms: i64,
}

#[derive(Debug, Deserialize)]
struct RuntimeStateEnvelope {
    snapshot: FirefoxSnapshot,
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
        write_native_session_heartbeat(&path, pid, now_unix_ms())?;

        let worker_path = path.clone();
        let (stop_tx, stop_rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("context-capsule-firefox-liveness".to_owned())
            .spawn(move || loop {
                match stop_rx.recv_timeout(NATIVE_SESSION_HEARTBEAT_INTERVAL) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let _ = write_native_session_heartbeat(
                            &worker_path,
                            pid,
                            now_unix_ms(),
                        );
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

/// Runs the proven Firefox/Zen native protocol while publishing a lightweight
/// liveness lease that is independent from semantic browser-state freshness.
///
/// A browser snapshot may remain unchanged for a long time. The native host
/// being actively connected is the evidence that the extension is still there
/// to report changes; rewriting the full snapshot on every native ping is both
/// unnecessary and excessively write-heavy.
pub fn run_native_host() -> Result<(), BrowserError> {
    // A user can launch capsule-firefox-host with no arguments for diagnostics.
    // Only the browser-style native-messaging invocation is allowed to prove
    // adapter liveness; otherwise a manually started host could keep stale tabs
    // eligible for capture even though no extension is connected.
    let lease = if is_native_messaging_invocation() {
        match NativeSessionLease::start() {
            Ok(lease) => Some(lease),
            Err(error) => {
                logging::warn(
                    "firefox",
                    format!("native session liveness lease is unavailable: {error}"),
                );
                None
            }
        }
    } else {
        None
    };

    let result = base::run_native_host();
    drop(lease);
    result
}

/// Returns the latest validated Firefox/Zen semantic snapshot.
///
/// The historical 90-second snapshot age rule remains the default. If that
/// rule reports the state as stale, an older snapshot is accepted only while a
/// currently running native-host session has a fresh liveness lease. This
/// separates "the browser state did not change" from "the adapter is gone".
pub fn load_recent_firefox_state() -> Result<Option<FirefoxSnapshot>, BrowserError> {
    match base::load_recent_firefox_state()? {
        Some(snapshot) => return Ok(Some(snapshot)),
        None => {}
    }

    let state_path = runtime_state_path()?;
    if !has_live_native_session_for_state(&state_path, now_unix_ms())? {
        return Ok(None);
    }

    // A newly connected extension publishes state immediately, but process
    // scheduling can still put the CLI preflight a few milliseconds ahead of
    // that first update. Wait only when a real live native session is already
    // proven, and keep the grace window bounded so missing/broken adapters fail
    // quickly instead of masking the completeness guard.
    let deadline = Instant::now() + NATIVE_SESSION_SYNC_GRACE;
    loop {
        match load_validated_state_ignoring_age(&state_path)? {
            Some(snapshot) => return Ok(Some(snapshot)),
            None if Instant::now() < deadline => thread::sleep(NATIVE_SESSION_SYNC_POLL),
            None => return Ok(None),
        }
    }
}

fn is_native_messaging_invocation() -> bool {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    is_native_messaging_arguments(&arguments)
}

fn is_native_messaging_arguments(arguments: &[String]) -> bool {
    if arguments.len() != 2 || arguments[1] != FIREFOX_EXTENSION_ID {
        return false;
    }
    Path::new(&arguments[0])
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.eq_ignore_ascii_case(&format!("{NATIVE_HOST_NAME}.json"))
        })
}

fn native_session_path(pid: u32) -> Result<PathBuf, BrowserError> {
    let state_path = runtime_state_path()?;
    let directory = state_path.parent().ok_or_else(|| {
        BrowserError::Invalid("Firefox runtime state path has no parent directory".to_owned())
    })?;
    Ok(directory.join(format!("{NATIVE_SESSION_PREFIX}{pid}.json")))
}

fn write_native_session_heartbeat(
    path: &Path,
    pid: u32,
    updated_at_unix_ms: i64,
) -> Result<(), BrowserError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let heartbeat = NativeSessionHeartbeat {
        pid,
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

fn has_live_native_session_for_state(
    state_path: &Path,
    now_ms: i64,
) -> Result<bool, BrowserError> {
    let Some(directory) = state_path.parent() else {
        return Ok(false);
    };
    has_live_native_session_in(directory, now_ms)
}

fn has_live_native_session_in(directory: &Path, now_ms: i64) -> Result<bool, BrowserError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(BrowserError::Io(error)),
    };

    let max_age_ms = NATIVE_SESSION_MAX_AGE.as_millis() as i64;
    let mut live = false;
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
            live = true;
        } else {
            let _ = fs::remove_file(path);
        }
    }
    Ok(live)
}

fn load_validated_state_ignoring_age(
    path: &Path,
) -> Result<Option<FirefoxSnapshot>, BrowserError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(BrowserError::Io(error)),
    };
    let envelope: RuntimeStateEnvelope = serde_json::from_slice(&bytes)?;
    validate_snapshot(&envelope.snapshot)?;
    Ok(Some(envelope.snapshot))
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

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "context-capsule-browser-live-{label}-{}-{}",
            std::process::id(),
            now_unix_ms()
        ))
    }

    fn sample_snapshot() -> FirefoxSnapshot {
        FirefoxSnapshot {
            schema_version: BROWSER_SNAPSHOT_SCHEMA_VERSION,
            browser: "firefox".to_owned(),
            extension_version: "test".to_owned(),
            captured_at_unix_ms: 1,
            skipped_private_windows: 0,
            windows: Vec::new(),
        }
    }

    #[test]
    fn only_browser_style_native_arguments_can_prove_liveness() {
        assert!(is_native_messaging_arguments(&[
            format!("/runtime/{NATIVE_HOST_NAME}.json"),
            FIREFOX_EXTENSION_ID.to_owned(),
        ]));
        assert!(!is_native_messaging_arguments(&[]));
        assert!(!is_native_messaging_arguments(&[
            format!("/runtime/{NATIVE_HOST_NAME}.json"),
            "wrong@extension.invalid".to_owned(),
        ]));
        assert!(!is_native_messaging_arguments(&[
            "/runtime/other-host.json".to_owned(),
            FIREFOX_EXTENSION_ID.to_owned(),
        ]));
    }

    #[test]
    fn live_native_session_is_recognized_and_stale_lease_is_removed() {
        let directory = temp_dir("lease");
        fs::create_dir_all(&directory).unwrap();
        let live = directory.join(format!("{NATIVE_SESSION_PREFIX}10.json"));
        let stale = directory.join(format!("{NATIVE_SESSION_PREFIX}11.json"));
        write_native_session_heartbeat(&live, 10, 100_000).unwrap();
        write_native_session_heartbeat(
            &stale,
            11,
            100_000 - NATIVE_SESSION_MAX_AGE.as_millis() as i64 - 1,
        )
        .unwrap();

        assert!(has_live_native_session_in(&directory, 100_000).unwrap());
        assert!(live.exists());
        assert!(!stale.exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn expired_native_session_does_not_keep_browser_state_live() {
        let directory = temp_dir("expired");
        fs::create_dir_all(&directory).unwrap();
        let stale = directory.join(format!("{NATIVE_SESSION_PREFIX}12.json"));
        write_native_session_heartbeat(
            &stale,
            12,
            100_000 - NATIVE_SESSION_MAX_AGE.as_millis() as i64 - 1,
        )
        .unwrap();
        assert!(!has_live_native_session_in(&directory, 100_000).unwrap());
        assert!(!stale.exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn stale_state_fallback_still_validates_snapshot_payload() {
        let directory = temp_dir("state");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("firefox.json");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "updated_at_unix_ms": 1,
                "snapshot": sample_snapshot(),
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            load_validated_state_ignoring_age(&path)
                .unwrap()
                .unwrap()
                .browser,
            "firefox"
        );

        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "updated_at_unix_ms": 1,
                "snapshot": {
                    "schema_version": 1,
                    "browser": "chrome",
                    "extension_version": "test",
                    "captured_at_unix_ms": 1,
                    "skipped_private_windows": 0,
                    "windows": []
                }
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(load_validated_state_ignoring_age(&path).is_err());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn missing_state_is_not_invented_even_when_adapter_is_live() {
        let directory = temp_dir("missing");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("firefox.json");
        assert!(load_validated_state_ignoring_age(&path).unwrap().is_none());
        let _ = fs::remove_dir_all(directory);
    }
}
