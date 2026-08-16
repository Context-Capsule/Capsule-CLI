use crate::{
    adapters::docker::{self, DockerSnapshot},
    desktop::{self, DesktopSnapshot},
    git::{self, GitContext, GitDiscoveryError},
    system::{self, SystemInfo},
    toolchain::{self, ToolVersion, VersionHint},
};
use std::{env, path::PathBuf};

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
}

pub fn discover(
    include_tools: bool,
    include_desktop: bool,
    include_docker: bool,
) -> Result<DiscoverySnapshot, String> {
    let current_directory = env::current_dir()
        .map_err(|error| format!("failed to determine current directory: {error}"))?;
    let system = system::discover();

    let git = match git::discover_current() {
        Ok(context) => GitState::Context(context),
        Err(GitDiscoveryError::NotInstalled) => GitState::GitUnavailable,
        Err(GitDiscoveryError::NotRepository) => GitState::NotRepository,
    };

    let project_root = match &git {
        GitState::Context(context) => PathBuf::from(&context.repository_root),
        GitState::NotRepository | GitState::GitUnavailable => current_directory.clone(),
    };

    let (tools, version_hints) = if include_tools {
        (
            toolchain::discover_tools(),
            toolchain::discover_version_hints(&project_root),
        )
    } else {
        (Vec::new(), Vec::new())
    };

    let desktop = if include_desktop {
        desktop::discover()
    } else {
        Err("desktop discovery was not requested".to_owned())
    };

    let docker = if include_docker {
        docker::discover()
    } else {
        DockerSnapshot::not_requested()
    };

    Ok(DiscoverySnapshot {
        current_directory,
        system,
        git,
        tools,
        version_hints,
        desktop,
        docker,
    })
}
