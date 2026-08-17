use super::{SavedApplication, SavedDesktop};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZenBootstrapReport {
    pub already_running: bool,
    pub planned: bool,
    pub launched: bool,
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

    match crate::desktop::application_running_by_executable_name(&["zen.exe", "zen"]) {
        Ok(true) => {
            report.already_running = true;
            return report;
        }
        Ok(false) => {}
        Err(error) => {
            report.failures.push(format!(
                "Zen bootstrap: could not inspect whether Zen is already running: {error}"
            ));
            return report;
        }
    }

    let Some(executable) = safe_zen_executable(application) else {
        report.failures.push(
            "Zen bootstrap: the saved browser is closed and its capsule entry has no safe zen.exe launch target"
                .to_owned(),
        );
        return report;
    };

    report.planned = true;
    if dry_run {
        report.warnings.push(format!(
            "Zen bootstrap: would start a native blank Zen window using '{executable}' before semantic browser restore"
        ));
        return report;
    }

    match Command::new(executable)
        .arg("--blank-window")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => {
            report.launched = true;
            report.warnings.push(
                "Zen bootstrap: started one native blank window because Zen was fully closed; semantic browser restore owns all saved tab/window reconstruction"
                    .to_owned(),
            );
        }
        Err(error) => report.failures.push(format!(
            "Zen bootstrap: failed to launch '{executable} --blank-window': {error}"
        )),
    }

    report
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
}
