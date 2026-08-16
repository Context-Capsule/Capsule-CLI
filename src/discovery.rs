use crate::{
    desktop::{self, DesktopSnapshot},
    git::{self, GitContext, GitDiscoveryError},
    toolchain::{self, ToolVersion},
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
    pub git: GitState,
    pub tools: Vec<ToolVersion>,
    pub desktop: Result<DesktopSnapshot, String>,
}

pub fn discover(include_tools: bool, include_desktop: bool) -> Result<DiscoverySnapshot, String> {
    let current_directory = env::current_dir()
        .map_err(|error| format!("failed to determine current directory: {error}"))?;

    let git = match git::discover_current() {
        Ok(context) => GitState::Context(context),
        Err(GitDiscoveryError::NotInstalled) => GitState::GitUnavailable,
        Err(GitDiscoveryError::NotRepository) => GitState::NotRepository,
    };

    let tools = if include_tools {
        toolchain::discover_tools()
    } else {
        Vec::new()
    };

    let desktop = if include_desktop {
        desktop::discover()
    } else {
        Err("desktop discovery was not requested".to_owned())
    };

    Ok(DiscoverySnapshot {
        current_directory,
        git,
        tools,
        desktop,
    })
}
