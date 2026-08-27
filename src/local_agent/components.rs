use crate::local_agent::{
    AgentError,
    protocol::{AgentSubsystem, CliInvocation},
};
use std::{
    env,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const WORKER_BINARY: &str = "capsule-agent-worker";
const MAX_CAPTURED_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct CaptureEngine;

impl CaptureEngine {
    fn accepts(&self, args: &[String]) -> bool {
        match args.first().map(String::as_str) {
            Some("inspect" | "save" | "update" | "apps" | "terminal") => true,
            Some("docker") => args.get(1).is_some_and(|value| value == "inspect"),
            _ => false,
        }
    }
}

#[derive(Debug, Default)]
pub struct RestoreEngine;

impl RestoreEngine {
    fn accepts(&self, args: &[String]) -> bool {
        match args.first().map(String::as_str) {
            Some("restore") => true,
            Some("docker") => args.get(1).is_some_and(|value| value == "restore"),
            _ => false,
        }
    }
}

#[derive(Debug, Default)]
pub struct SqliteService;

impl SqliteService {
    fn accepts(&self, args: &[String]) -> bool {
        matches!(
            args.first().map(String::as_str),
            Some("list" | "history" | "show" | "note" | "diff" | "delete")
        )
    }
}

#[derive(Debug)]
enum WorkerLaunch {
    Binary(PathBuf),
    Cargo {
        manifest: PathBuf,
        target_dir: PathBuf,
    },
}

#[derive(Debug)]
pub struct AdapterHost {
    worker: WorkerLaunch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionResult {
    pub subsystem: AgentSubsystem,
    pub exit_code: u8,
    pub stdout: String,
    pub stderr: String,
}

impl AdapterHost {
    pub fn discover() -> Result<Self, AgentError> {
        let executable = env::current_exe().map_err(|error| {
            AgentError::Runtime(format!("could not resolve Local Agent executable: {error}"))
        })?;
        let sibling = sibling_worker_path(&executable);
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let cargo_worker = || WorkerLaunch::Cargo {
            target_dir: cargo_worker_target_dir(&manifest),
            manifest: manifest.clone(),
        };

        // In debug/development builds always go through Cargo when the source
        // manifest is available. That prevents a previously-built worker from
        // becoming stale when only worker-owned source files change between
        // `cargo run` invocations. The worker uses its own target directory so
        // a CLI that itself was launched by Cargo cannot deadlock on Cargo's
        // target-directory lock. Release/install builds use the sibling worker
        // directly and pay no Cargo startup cost.
        if cfg!(debug_assertions) && manifest.is_file() {
            return Ok(Self {
                worker: cargo_worker(),
            });
        }
        if sibling.is_file() {
            return Ok(Self {
                worker: WorkerLaunch::Binary(sibling),
            });
        }

        // A release-like development invocation may still have built only the
        // public `capsule` target. Lazily build the worker from the same
        // manifest in its isolated cache so existing `cargo run` smoke tests
        // keep working without contending with the parent Cargo process.
        if manifest.is_file() {
            return Ok(Self {
                worker: cargo_worker(),
            });
        }

        Err(AgentError::Runtime(format!(
            "Local Agent worker is missing at {}; reinstall Context Capsule with both capsule and {}",
            sibling.display(),
            executable_name(WORKER_BINARY)
        )))
    }

    fn execute(
        &self,
        invocation: &CliInvocation,
        subsystem: AgentSubsystem,
    ) -> Result<ExecutionResult, AgentError> {
        if invocation.current_directory.trim().is_empty() {
            return Err(AgentError::Protocol(
                "CLI invocation did not include a working directory".to_owned(),
            ));
        }

        let mut command = match &self.worker {
            WorkerLaunch::Binary(path) => Command::new(path),
            WorkerLaunch::Cargo {
                manifest,
                target_dir,
            } => {
                let mut command = Command::new("cargo");
                command
                    .arg("run")
                    .arg("--quiet")
                    .arg("--manifest-path")
                    .arg(manifest)
                    .arg("--target-dir")
                    .arg(target_dir)
                    .arg("--bin")
                    .arg(WORKER_BINARY)
                    .arg("--");
                command
            }
        };

        command
            .args(&invocation.args)
            .current_dir(&invocation.current_directory)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for entry in &invocation.environment {
            if !entry.key.is_empty() {
                command.env(&entry.key, &entry.value);
            }
        }

        let output = command.output().map_err(|error| {
            AgentError::Runtime(format!("Local Agent worker could not start: {error}"))
        })?;
        let exit_code = output
            .status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(1);

        Ok(ExecutionResult {
            subsystem,
            exit_code,
            stdout: bounded_text(output.stdout),
            stderr: bounded_text(output.stderr),
        })
    }
}

fn sibling_worker_path(current_executable: &Path) -> PathBuf {
    current_executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(executable_name(WORKER_BINARY))
}

fn cargo_worker_target_dir(manifest: &Path) -> PathBuf {
    manifest
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("target")
        .join("local-agent-worker")
}

fn executable_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_owned()
    }
}

fn bounded_text(mut bytes: Vec<u8>) -> String {
    if bytes.len() > MAX_CAPTURED_OUTPUT_BYTES {
        bytes.truncate(MAX_CAPTURED_OUTPUT_BYTES);
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        text.push_str("\n[Context Capsule Local Agent truncated command output]\n");
        return text;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[derive(Debug)]
pub struct LocalAgentRuntime {
    capture_engine: CaptureEngine,
    restore_engine: RestoreEngine,
    sqlite: SqliteService,
    adapter_host: AdapterHost,
}

impl LocalAgentRuntime {
    pub fn new() -> Result<Self, AgentError> {
        Ok(Self {
            capture_engine: CaptureEngine,
            restore_engine: RestoreEngine,
            sqlite: SqliteService,
            adapter_host: AdapterHost::discover()?,
        })
    }

    pub fn execute(&self, invocation: &CliInvocation) -> Result<ExecutionResult, AgentError> {
        let subsystem = self.route(&invocation.args);
        self.adapter_host.execute(invocation, subsystem)
    }

    pub fn route(&self, args: &[String]) -> AgentSubsystem {
        if self.restore_engine.accepts(args) {
            AgentSubsystem::RestoreEngine
        } else if self.capture_engine.accepts(args) {
            AgentSubsystem::CaptureEngine
        } else if self.sqlite.accepts(args) {
            AgentSubsystem::Sqlite
        } else {
            // `doctor`, help, invalid invocations, and future adapter-oriented
            // commands are intentionally hosted at the adapter boundary.
            AgentSubsystem::AdapterHost
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn runtime() -> LocalAgentRuntime {
        LocalAgentRuntime {
            capture_engine: CaptureEngine,
            restore_engine: RestoreEngine,
            sqlite: SqliteService,
            adapter_host: AdapterHost {
                worker: WorkerLaunch::Binary(PathBuf::from("capsule-agent-worker")),
            },
        }
    }

    #[test]
    fn command_domains_match_the_local_agent_architecture() {
        let runtime = runtime();
        assert_eq!(runtime.route(&args(&["save", "work"])), AgentSubsystem::CaptureEngine);
        assert_eq!(runtime.route(&args(&["inspect", "--tools"])), AgentSubsystem::CaptureEngine);
        assert_eq!(runtime.route(&args(&["restore", "work"])), AgentSubsystem::RestoreEngine);
        assert_eq!(runtime.route(&args(&["docker", "restore", "work"])), AgentSubsystem::RestoreEngine);
        assert_eq!(runtime.route(&args(&["show", "work"])), AgentSubsystem::Sqlite);
        assert_eq!(runtime.route(&args(&["doctor"])), AgentSubsystem::AdapterHost);
    }

    #[test]
    fn docker_inspect_is_capture_but_docker_restore_is_restore() {
        let capture = CaptureEngine;
        let restore = RestoreEngine;
        assert!(capture.accepts(&args(&["docker", "inspect"])));
        assert!(!capture.accepts(&args(&["docker", "restore", "work"])));
        assert!(restore.accepts(&args(&["docker", "restore", "work"])));
    }

    #[test]
    fn sibling_worker_uses_platform_executable_suffix() {
        let current = Path::new("/tmp/context-capsule/capsule");
        let worker = sibling_worker_path(current);
        assert!(worker.to_string_lossy().contains("capsule-agent-worker"));
    }

    #[test]
    fn cargo_worker_uses_an_isolated_target_directory() {
        let target = cargo_worker_target_dir(Path::new("/tmp/context-capsule/Cargo.toml"));
        assert!(target.ends_with("target/local-agent-worker"));
    }
}
