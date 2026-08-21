use super::{SavedApplication, SavedDesktop};
use std::process::{Command, Stdio};

// Use Firefox/Zen's documented `--new-window <url>` contract for the cold
// bootstrap. `--blank-window` is intentionally not used here: Zen can surface
// that special window without an ordinary tab, which is exactly the state in
// which the WebExtension/native-messaging adapter failed to wake reliably.
// A normal HTTPS URL creates a real tab even if the machine is offline (the tab
// may show a network error, but it still exists and activates browser add-ons).
const COLD_BOOTSTRAP_URL: &str = "https://example.com/?context-capsule-bootstrap=1";

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

    match zen_has_visible_window() {
        Ok(true) => {
            // Preserve the proven warm-restore path only when an actual visible
            // Zen window exists. A surviving background Zen process with no
            // windows is still a cold restore and needs a real tab to wake the
            // extension/native-messaging adapter.
            report.already_running = true;
            return report;
        }
        Ok(false) => {}
        Err(error) => {
            report.failures.push(format!(
                "Zen bootstrap: could not inspect whether Zen has a visible window: {error}"
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

    // This path runs only when desktop discovery found no visible Zen window,
    // so there is no existing visible Zen Space for a new-window launch to
    // clone or mutate. Starting a standard window with a standard URL gives us
    // the strongest browser-level guarantee available that a real tab exists.
    // Once the extension receives the semantic restore request, its
    // authoritative preparation removes this bootstrap tab/window state and
    // replaces it with the capsule topology.
    match Command::new(executable)
        .arg("--new-window")
        .arg(COLD_BOOTSTRAP_URL)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => {
            report.launched = true;

            // Do not gate semantic restore on a fresh browser.state.update.
            // The restore bus is the authoritative handshake: after bootstrap,
            // the CLI writes a concrete Firefox restore request and waits for the
            // extension to acknowledge completion. If the extension/native host
            // is actually unavailable, that request times out with persistent
            // firefox.log diagnostics.
        }
        Err(error) => {
            report.failures.push(format!(
                "Zen bootstrap: failed to launch '{executable} --new-window {COLD_BOOTSTRAP_URL}': {error}"
            ));
            report.skip_semantic_restore = true;
        }
    }

    report
}

fn zen_has_visible_window() -> Result<bool, String> {
    let snapshot = crate::desktop::discover()?;
    Ok(snapshot.applications.iter().any(|application| {
        application
            .executable_path
            .as_deref()
            .and_then(executable_basename)
            .is_some_and(|name| {
                name.eq_ignore_ascii_case("zen.exe") || name.eq_ignore_ascii_case("zen")
            })
            && !application.windows.is_empty()
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
    fn cold_bootstrap_uses_a_real_standard_url_tab() {
        assert!(COLD_BOOTSTRAP_URL.starts_with("https://"));
        assert!(COLD_BOOTSTRAP_URL.contains("context-capsule-bootstrap"));
    }
}
