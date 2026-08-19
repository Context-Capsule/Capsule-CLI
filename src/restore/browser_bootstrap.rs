use super::{SavedApplication, SavedDesktop};
use serde_json::Value;
use std::{
    fs,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const ADAPTER_READY_TIMEOUT: Duration = Duration::from_secs(20);
const ADAPTER_READY_POLL: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZenBootstrapReport {
    pub already_running: bool,
    pub planned: bool,
    pub launched: bool,
    pub skip_semantic_restore: bool,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

pub fn ensure_zen_started(saved: &SavedDesktop, dry_run: bool) -> ZenBootstrapReport {
    let mut report = ZenBootstrapReport::default();
    let Some(application) = saved
        .applications
        .iter()
        .find(|application| is_zen_application(application))
    else {
        return report;
    };

    match zen_is_running() {
        Ok(true) => {
            // Preserve the proven warm-restore path. The semantic adapter itself
            // still owns the final request/timeout diagnostics in this case.
            report.already_running = true;
            return report;
        }
        Ok(false) => {}
        Err(error) => {
            report.failures.push(format!(
                "Zen bootstrap: could not inspect whether Zen is already running: {error}"
            ));
            report.skip_semantic_restore = true;
            return report;
        }
    }

    let Some(executable) = safe_zen_executable(application) else {
        report.failures.push(
            "Zen bootstrap: the saved browser is closed and its capsule entry has no safe zen.exe launch target"
                .to_owned(),
        );
        report.skip_semantic_restore = true;
        return report;
    };

    report.planned = true;
    if dry_run {
        return report;
    }

    // A stale firefox.json from before Zen was closed is not proof that the
    // extension survived the restart. Record the previous heartbeat and launch
    // time, then require a genuinely newer state write before semantic restore.
    let previous_heartbeat = adapter_state_updated_at_unix_ms().ok().flatten();
    let launched_after_unix_ms = now_unix_ms();
    match Command::new(executable)
        .arg("--blank-window")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => report.launched = true,
        Err(error) => {
            report.failures.push(format!(
                "Zen bootstrap: failed to launch '{executable} --blank-window': {error}"
            ));
            report.skip_semantic_restore = true;
            return report;
        }
    }

    match wait_for_fresh_adapter_heartbeat(
        previous_heartbeat,
        launched_after_unix_ms,
        ADAPTER_READY_TIMEOUT,
    ) {
        Ok(true) => {}
        Ok(false) => {
            report.skip_semantic_restore = true;
            report.failures.push(format!(
                "Zen bootstrap: Zen started, but the Context Capsule Firefox/Zen adapter did not publish a fresh heartbeat within {} seconds. The semantic restore was not attempted. If the extension was loaded temporarily from about:debugging, a full Zen shutdown removes that temporary installation; load/install the extension again (or use a persistent packaged installation) and retry. See firefox.log for adapter startup diagnostics.",
                ADAPTER_READY_TIMEOUT.as_secs()
            ));
        }
        Err(error) => {
            report.skip_semantic_restore = true;
            report.failures.push(format!(
                "Zen bootstrap: Zen started, but Context Capsule could not verify that the Firefox/Zen adapter reconnected: {error}. The semantic restore was not attempted."
            ));
        }
    }

    report
}

fn wait_for_fresh_adapter_heartbeat(
    previous_heartbeat: Option<i64>,
    launched_after_unix_ms: i64,
    timeout: Duration,
) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    let mut last_error = None;

    loop {
        match adapter_state_updated_at_unix_ms() {
            Ok(Some(updated_at))
                if adapter_heartbeat_is_fresh(
                    updated_at,
                    previous_heartbeat,
                    launched_after_unix_ms,
                ) =>
            {
                return Ok(true);
            }
            Ok(_) => {
                last_error = None;
            }
            Err(error) => {
                // A partially-written/temporarily unavailable runtime file can
                // occur during startup. Keep polling until the deadline rather
                // than failing on a transient read while the extension writes.
                last_error = Some(error);
            }
        }

        if Instant::now() >= deadline {
            return last_error.map_or(Ok(false), Err);
        }
        thread::sleep(ADAPTER_READY_POLL);
    }
}

fn adapter_heartbeat_is_fresh(
    updated_at_unix_ms: i64,
    previous_heartbeat: Option<i64>,
    launched_after_unix_ms: i64,
) -> bool {
    let newer_than_previous = previous_heartbeat
        .map(|previous| updated_at_unix_ms > previous)
        .unwrap_or(true);
    newer_than_previous && updated_at_unix_ms >= launched_after_unix_ms
}

fn adapter_state_updated_at_unix_ms() -> Result<Option<i64>, String> {
    let path = crate::browser::runtime_state_path().map_err(|error| error.to_string())?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "could not read Firefox adapter state '{}': {error}",
                path.display()
            ));
        }
    };
    let envelope: Value = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "Firefox adapter state '{}' is invalid JSON: {error}",
            path.display()
        )
    })?;
    Ok(envelope
        .get("updated_at_unix_ms")
        .and_then(Value::as_i64))
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn zen_is_running() -> Result<bool, String> {
    let snapshot = crate::desktop::discover()?;
    Ok(snapshot.applications.iter().any(|application| {
        application
            .executable_path
            .as_deref()
            .and_then(executable_basename)
            .is_some_and(|name| {
                name.eq_ignore_ascii_case("zen.exe") || name.eq_ignore_ascii_case("zen")
            })
    }))
}

fn is_zen_application(application: &SavedApplication) -> bool {
    application
        .executable_path
        .as_deref()
        .or_else(|| application.launch.as_ref().map(|launch| launch.target.as_str()))
        .and_then(executable_basename)
        .is_some_and(|name| {
            name.eq_ignore_ascii_case("zen.exe") || name.eq_ignore_ascii_case("zen")
        })
        || application.name.eq_ignore_ascii_case("zen")
        || application.name.eq_ignore_ascii_case("Zen Browser")
}

fn safe_zen_executable(application: &SavedApplication) -> Option<&str> {
    let candidate = application
        .executable_path
        .as_deref()
        .or_else(|| application.launch.as_ref().map(|launch| launch.target.as_str()))?;
    let basename = executable_basename(candidate)?;
    (basename.eq_ignore_ascii_case("zen.exe") || basename.eq_ignore_ascii_case("zen"))
        .then_some(candidate)
}

fn executable_basename(path: &str) -> Option<&str> {
    path.rsplit(['\\', '/'])
        .next()
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn application(name: &str, executable: Option<&str>) -> SavedApplication {
        SavedApplication {
            name: name.to_owned(),
            executable_path: executable.map(str::to_owned),
            app_user_model_id: None,
            file_version: None,
            classification: "user-application".to_owned(),
            launch: None,
            windows: Vec::new(),
            discovered_as_background: false,
        }
    }

    #[test]
    fn zen_identity_accepts_windows_paths_on_every_test_host() {
        let zen = application("zen", Some(r"C:\Program Files\Zen Browser\zen.exe"));
        assert!(is_zen_application(&zen));
        assert_eq!(
            safe_zen_executable(&zen),
            Some(r"C:\Program Files\Zen Browser\zen.exe")
        );
    }

    #[test]
    fn zen_bootstrap_rejects_arbitrary_executables() {
        let malicious = application("Zen Browser", Some(r"C:\Windows\System32\cmd.exe"));
        assert!(is_zen_application(&malicious));
        assert_eq!(safe_zen_executable(&malicious), None);
    }

    #[test]
    fn heartbeat_must_be_newer_than_the_prelaunch_state_and_not_predate_launch() {
        let launched = 50_000_i64;
        let previous = Some(49_900_i64);
        assert!(!adapter_heartbeat_is_fresh(49_900, previous, launched));
        assert!(!adapter_heartbeat_is_fresh(49_999, previous, launched));
        assert!(adapter_heartbeat_is_fresh(50_000, previous, launched));
        assert!(adapter_heartbeat_is_fresh(50_050, previous, launched));
        assert!(!adapter_heartbeat_is_fresh(49_999, None, launched));
    }
}
