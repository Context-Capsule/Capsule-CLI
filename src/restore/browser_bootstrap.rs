use super::{SavedApplication, SavedDesktop};
use std::process::{Command, Stdio};

const COLD_BOOTSTRAP_URL: &str = "about:newtab";

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
            // Preserve the proven warm-restore path. The semantic adapter owns
            // the actual restore request and completion diagnostics.
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

    // A completely empty Zen `--blank-window` can exist without an ordinary tab.
    // In that state Zen may not activate WebExtensions/native messaging yet, so a
    // cold Context Capsule restore can wait forever even though the extension is
    // installed correctly. Keep Zen's independent unsynced blank-window mode, but
    // give it one disposable about:newtab page. The Firefox adapter already treats
    // about:newtab as a bootstrap tab and replaces it with the saved tabs.
    match Command::new(executable)
        .arg("--blank-window")
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
                "Zen bootstrap: failed to launch '{executable} --blank-window {COLD_BOOTSTRAP_URL}': {error}"
            ));
            report.skip_semantic_restore = true;
        }
    }

    report
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
    fn cold_bootstrap_uses_an_extension_waking_tab() {
        assert_eq!(COLD_BOOTSTRAP_URL, "about:newtab");
    }
}
