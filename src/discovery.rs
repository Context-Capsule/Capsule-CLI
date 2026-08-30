use crate::{
    adapters::{
        docker::{self, DockerSnapshot},
        terminal::{self, TerminalSnapshot},
    },
    desktop::{self, DesktopSnapshot},
    git::{self, GitContext, GitDiscoveryError},
    logging,
    system::{self, SystemInfo},
    toolchain::{self, ToolVersion, VersionHint},
};
use std::{env, path::PathBuf, thread, time::Instant};

const PERFORMANCE_LOG_COMPONENT: &str = "performance";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitState {
    Context(GitContext),
    NotRepository,
    GitUnavailable,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiscoverySnapshot {
    pub current_directory: PathBuf,
    pub system: SystemInfo,
    pub git: GitState,
    pub tools: Vec<ToolVersion>,
    pub version_hints: Vec<VersionHint>,
    pub desktop: Result<DesktopSnapshot, String>,
    pub docker: DockerSnapshot,
    pub terminals: TerminalSnapshot,
}

pub fn discover(
    include_tools: bool,
    include_desktop: bool,
    include_docker: bool,
    include_terminals: bool,
) -> Result<DiscoverySnapshot, String> {
    let started = Instant::now();
    logging::info(
        PERFORMANCE_LOG_COMPONENT,
        format!(
            "discovery.begin tools={include_tools} desktop={include_desktop} docker={include_docker} terminals={include_terminals}"
        ),
    );

    let phase = Instant::now();
    let current_directory = env::current_dir()
        .map_err(|error| format!("failed to determine current directory: {error}"))?;
    logging::info(
        PERFORMANCE_LOG_COMPONENT,
        format!(
            "discovery.phase current_directory elapsed_ms={}",
            phase.elapsed().as_millis()
        ),
    );

    let phase = Instant::now();
    let system = system::discover();
    logging::info(
        PERFORMANCE_LOG_COMPONENT,
        format!(
            "discovery.phase system elapsed_ms={}",
            phase.elapsed().as_millis()
        ),
    );

    // Git discovery establishes the project root used for version-hint files, so
    // keep it ahead of the independent adapter probes below.
    let phase = Instant::now();
    let git = match git::discover_current() {
        Ok(context) => GitState::Context(context),
        Err(GitDiscoveryError::NotInstalled) => GitState::GitUnavailable,
        Err(GitDiscoveryError::NotRepository) => GitState::NotRepository,
    };
    logging::info(
        PERFORMANCE_LOG_COMPONENT,
        format!(
            "discovery.phase git elapsed_ms={}",
            phase.elapsed().as_millis()
        ),
    );

    let project_root = match &git {
        GitState::Context(context) => PathBuf::from(&context.repository_root),
        GitState::NotRepository | GitState::GitUnavailable => current_directory.clone(),
    };

    // Toolchain, desktop, Docker, and terminal discovery are read-only snapshots
    // with no data dependency on one another. Running them concurrently removes
    // avoidable wall-clock serialization while preserving each adapter's existing
    // implementation, timeout, fallback, and safety behavior.
    let (
        ((tools, version_hints), tools_elapsed_ms),
        (desktop, desktop_elapsed_ms),
        (docker, docker_elapsed_ms),
        (terminals, terminals_elapsed_ms),
    ) = thread::scope(|scope| {
        let tools_handle = scope.spawn(|| {
            let phase = Instant::now();
            let result = if include_tools {
                (
                    toolchain::discover_tools(),
                    toolchain::discover_version_hints(&project_root),
                )
            } else {
                (Vec::new(), Vec::new())
            };
            (result, phase.elapsed().as_millis())
        });

        let desktop_handle = scope.spawn(|| {
            let phase = Instant::now();
            let result = if include_desktop {
                desktop::discover()
            } else {
                Err("desktop discovery was not requested".to_owned())
            };
            (result, phase.elapsed().as_millis())
        });

        let docker_handle = scope.spawn(|| {
            let phase = Instant::now();
            let result = if include_docker {
                docker::discover()
            } else {
                DockerSnapshot::not_requested()
            };
            (result, phase.elapsed().as_millis())
        });

        let terminals_handle = scope.spawn(|| {
            let phase = Instant::now();
            let result = if include_terminals {
                terminal::discover()
            } else {
                TerminalSnapshot::not_requested()
            };
            (result, phase.elapsed().as_millis())
        });

        (
            tools_handle.join().expect("toolchain discovery thread panicked"),
            desktop_handle.join().expect("desktop discovery thread panicked"),
            docker_handle.join().expect("Docker discovery thread panicked"),
            terminals_handle.join().expect("terminal discovery thread panicked"),
        )
    });

    logging::info(
        PERFORMANCE_LOG_COMPONENT,
        format!(
            "discovery.phase tools elapsed_ms={tools_elapsed_ms} enabled={include_tools} tools={} hints={}",
            tools.len(),
            version_hints.len()
        ),
    );
    logging::info(
        PERFORMANCE_LOG_COMPONENT,
        format!(
            "discovery.phase desktop elapsed_ms={desktop_elapsed_ms} enabled={include_desktop} ok={}",
            desktop.is_ok()
        ),
    );
    logging::info(
        PERFORMANCE_LOG_COMPONENT,
        format!(
            "discovery.phase docker elapsed_ms={docker_elapsed_ms} enabled={include_docker}"
        ),
    );
    logging::info(
        PERFORMANCE_LOG_COMPONENT,
        format!(
            "discovery.phase terminals elapsed_ms={terminals_elapsed_ms} enabled={include_terminals} sessions={}",
            terminals.sessions.len()
        ),
    );

    logging::info(
        PERFORMANCE_LOG_COMPONENT,
        format!(
            "discovery.complete elapsed_ms={}",
            started.elapsed().as_millis()
        ),
    );

    Ok(DiscoverySnapshot {
        current_directory,
        system,
        git,
        tools,
        version_hints,
        desktop,
        docker,
        terminals,
    })
}
