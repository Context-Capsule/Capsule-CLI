use crate::{
    adapters::docker::{self, DockerStatus},
    browser, logging,
    persistence::CapsuleStore,
    vscode,
};
use serde::Serialize;
use serde_json::Value;
use std::{fs, path::PathBuf, process::Command};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoctorStatus {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorCheck {
    pub component: String,
    pub status: DoctorStatus,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    pub version: String,
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn has_errors(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.status == DoctorStatus::Error)
    }

    pub fn warning_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == DoctorStatus::Warning)
            .count()
    }
}

pub fn run() -> DoctorReport {
    DoctorReport {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        checks: vec![
            database_check(),
            firefox_native_host_check(),
            firefox_adapter_check(),
            vscode_adapter_check(),
            git_check(),
            docker_check(),
            logging_check(),
        ],
    }
}

fn database_check() -> DoctorCheck {
    match CapsuleStore::open_default() {
        Ok(store) => match store.health_check() {
            Ok(()) => {
                let count = store.list().map(|capsules| capsules.len()).unwrap_or(0);
                check(
                    "Local database",
                    DoctorStatus::Ok,
                    "healthy",
                    vec![
                        format!("path: {}", store.path().display()),
                        format!("capsules: {count}"),
                    ],
                    None,
                )
            }
            Err(error) => check(
                "Local database",
                DoctorStatus::Error,
                format!("integrity check failed: {error}"),
                vec![format!("path: {}", store.path().display())],
                Some("Back up capsules.db before attempting any manual SQLite repair."),
            ),
        },
        Err(error) => check(
            "Local database",
            DoctorStatus::Error,
            format!("unavailable: {error}"),
            Vec::new(),
            Some("Check Context Capsule's data-directory permissions."),
        ),
    }
}

fn firefox_native_host_check() -> DoctorCheck {
    let manifest_path = match browser::native_manifest_path() {
        Ok(path) => path,
        Err(error) => {
            return check(
                "Firefox/Zen native host",
                DoctorStatus::Error,
                format!("manifest path unavailable: {error}"),
                Vec::new(),
                Some("Run capsule-firefox-host --install after fixing the environment."),
            );
        }
    };

    let bytes = match fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return check(
                "Firefox/Zen native host",
                DoctorStatus::Warning,
                "not installed",
                vec![format!("expected manifest: {}", manifest_path.display())],
                Some("Run capsule-firefox-host --install."),
            );
        }
        Err(error) => {
            return check(
                "Firefox/Zen native host",
                DoctorStatus::Error,
                format!("manifest unreadable: {error}"),
                vec![format!("manifest: {}", manifest_path.display())],
                Some("Reinstall with capsule-firefox-host --install."),
            );
        }
    };

    let manifest: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return check(
                "Firefox/Zen native host",
                DoctorStatus::Error,
                format!("invalid manifest JSON: {error}"),
                vec![format!("manifest: {}", manifest_path.display())],
                Some("Reinstall with capsule-firefox-host --install."),
            );
        }
    };

    let name_ok = manifest.get("name").and_then(Value::as_str) == Some(browser::NATIVE_HOST_NAME);
    let executable = manifest
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from);
    let executable_ok = executable.as_ref().is_some_and(|path| path.is_file());
    let extensions_ok = manifest
        .get("allowed_extensions")
        .and_then(Value::as_array)
        .is_some_and(|allowed| {
            allowed.len() == 1
                && allowed[0].as_str() == Some(browser::FIREFOX_EXTENSION_ID)
        });

    let mut details = vec![format!("manifest: {}", manifest_path.display())];
    if let Some(executable) = executable.as_ref() {
        details.push(format!("executable: {}", executable.display()));
    }

    if name_ok && executable_ok && extensions_ok {
        check(
            "Firefox/Zen native host",
            DoctorStatus::Ok,
            "manifest and executable are valid",
            details,
            Some("For a full stdio/registry probe, run capsule-firefox-host --doctor."),
        )
    } else {
        let mut failures = Vec::new();
        if !name_ok {
            failures.push("native host name does not match");
        }
        if !executable_ok {
            failures.push("native host executable is missing");
        }
        if !extensions_ok {
            failures.push("allowed_extensions is not restricted to Context Capsule");
        }
        check(
            "Firefox/Zen native host",
            DoctorStatus::Error,
            failures.join("; "),
            details,
            Some("Reinstall with capsule-firefox-host --install."),
        )
    }
}

fn firefox_adapter_check() -> DoctorCheck {
    match browser::load_recent_firefox_state() {
        Ok(Some(snapshot)) => check(
            "Firefox/Zen adapter",
            DoctorStatus::Ok,
            "live semantic state available",
            vec![
                format!("extension version: {}", snapshot.extension_version),
                format!("windows: {}", snapshot.windows.len()),
                format!("tabs: {}", snapshot.tab_count()),
                format!("private windows skipped: {}", snapshot.skipped_private_windows),
            ],
            None,
        ),
        Ok(None) => check(
            "Firefox/Zen adapter",
            DoctorStatus::Warning,
            "no recent semantic state",
            Vec::new(),
            Some("Open Zen/Firefox with the Context Capsule extension loaded, then wait a few seconds."),
        ),
        Err(error) => check(
            "Firefox/Zen adapter",
            DoctorStatus::Error,
            format!("state is invalid or unreadable: {error}"),
            Vec::new(),
            Some("Reload the browser extension and inspect its persistent Firefox log."),
        ),
    }
}

fn vscode_adapter_check() -> DoctorCheck {
    match vscode::load_recent_vscode_state() {
        Ok(Some(snapshot)) => {
            let mut details = vec![
                format!("tabs: {}", snapshot.tab_count()),
                format!("integrated terminals: {}", snapshot.integrated_terminals.len()),
            ];
            if let Some(mode) = snapshot.extension_mode.as_deref() {
                details.push(format!("host mode: {mode}"));
            }
            if let Some(detection) = snapshot.host_detection.as_deref() {
                details.push(format!("host detection: {detection}"));
            }
            if let Some(path) = snapshot.extension_path.as_deref() {
                details.push(format!("development path: {path}"));
            }
            check(
                "VS Code adapter",
                DoctorStatus::Ok,
                "live semantic state available",
                details,
                None,
            )
        }
        Ok(None) => check(
            "VS Code adapter",
            DoctorStatus::Warning,
            "no recent semantic state",
            Vec::new(),
            Some("Open a VS Code window with Context Capsule loaded and wait for its heartbeat."),
        ),
        Err(error) => check(
            "VS Code adapter",
            DoctorStatus::Error,
            format!("state is invalid or unreadable: {error}"),
            Vec::new(),
            Some("Reload the VS Code extension and inspect vscode-host-*.log."),
        ),
    }
}

fn git_check() -> DoctorCheck {
    match Command::new("git").arg("--version").output() {
        Ok(output) if output.status.success() => check(
            "Git",
            DoctorStatus::Ok,
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            Vec::new(),
            None,
        ),
        Ok(output) => check(
            "Git",
            DoctorStatus::Warning,
            format!("git --version exited with {}", output.status),
            Vec::new(),
            Some("Check that the expected Git installation is first on PATH."),
        ),
        Err(error) => check(
            "Git",
            DoctorStatus::Warning,
            format!("not available on PATH: {error}"),
            Vec::new(),
            Some("Install Git or add git.exe to PATH."),
        ),
    }
}

fn docker_check() -> DoctorCheck {
    let snapshot = docker::discover();
    match snapshot.status {
        DockerStatus::Available => check(
            "Docker",
            DoctorStatus::Ok,
            format!("{} running container(s)", snapshot.running_container_count()),
            snapshot
                .context
                .into_iter()
                .map(|context| format!("context: {context}"))
                .collect(),
            None,
        ),
        DockerStatus::Unavailable => check(
            "Docker",
            DoctorStatus::Warning,
            snapshot
                .message
                .unwrap_or_else(|| "Docker is unavailable".to_owned()),
            Vec::new(),
            Some("Start Docker if this capsule is expected to restore container resources."),
        ),
        DockerStatus::NotRequested => check(
            "Docker",
            DoctorStatus::Warning,
            "Docker was not inspected",
            Vec::new(),
            None,
        ),
    }
}

fn logging_check() -> DoctorCheck {
    match logging::log_directory() {
        Ok(path) => match fs::create_dir_all(&path) {
            Ok(()) => check(
                "Logging",
                DoctorStatus::Ok,
                "persistent component logs available",
                vec![
                    format!("directory: {}", path.display()),
                    "rotation: 1 MiB per component, one previous file retained".to_owned(),
                ],
                None,
            ),
            Err(error) => check(
                "Logging",
                DoctorStatus::Error,
                format!("cannot create log directory: {error}"),
                vec![format!("directory: {}", path.display())],
                Some("Check filesystem permissions for the Context Capsule state directory."),
            ),
        },
        Err(error) => check(
            "Logging",
            DoctorStatus::Error,
            format!("log directory unavailable: {error}"),
            Vec::new(),
            Some("Set CONTEXT_CAPSULE_LOG_DIR to a writable directory."),
        ),
    }
}

fn check(
    component: impl Into<String>,
    status: DoctorStatus,
    summary: impl Into<String>,
    details: Vec<String>,
    hint: Option<&str>,
) -> DoctorCheck {
    DoctorCheck {
        component: component.into(),
        status,
        summary: summary.into(),
        details,
        hint: hint.map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_helpers_distinguish_warnings_from_errors() {
        let report = DoctorReport {
            version: "test".to_owned(),
            checks: vec![
                check("one", DoctorStatus::Ok, "ok", Vec::new(), None),
                check(
                    "two",
                    DoctorStatus::Warning,
                    "warning",
                    Vec::new(),
                    None,
                ),
            ],
        };
        assert!(!report.has_errors());
        assert_eq!(report.warning_count(), 1);

        let report = DoctorReport {
            version: "test".to_owned(),
            checks: vec![check(
                "bad",
                DoctorStatus::Error,
                "error",
                Vec::new(),
                None,
            )],
        };
        assert!(report.has_errors());
    }

    #[test]
    fn doctor_status_serializes_stably() {
        assert_eq!(serde_json::to_string(&DoctorStatus::Ok).unwrap(), "\"ok\"");
        assert_eq!(
            serde_json::to_string(&DoctorStatus::Warning).unwrap(),
            "\"warning\""
        );
        assert_eq!(
            serde_json::to_string(&DoctorStatus::Error).unwrap(),
            "\"error\""
        );
    }
}
