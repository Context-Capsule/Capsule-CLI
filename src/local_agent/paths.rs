use crate::{local_agent::AgentError, persistence};
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

const STATE_FILE: &str = "local-agent-v1.json";
const LOCK_FILE: &str = "local-agent-v1.lock";
const LOG_FILE: &str = "local-agent.log";

pub fn state_directory() -> Result<PathBuf, AgentError> {
    if let Ok(database) = persistence::default_database_path()
        && let Some(parent) = database.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
        return Ok(parent.to_path_buf());
    }

    // Discovery commands historically did not require the persistence path to
    // be available. Keep that behavior by giving the agent a temp fallback if
    // HOME/LOCALAPPDATA/XDG state is unavailable; persistence commands will
    // still report their original database error inside the worker.
    let fallback = env::temp_dir().join("ContextCapsule");
    fs::create_dir_all(&fallback)?;
    Ok(fallback)
}

pub fn state_path() -> Result<PathBuf, AgentError> {
    Ok(state_directory()?.join(STATE_FILE))
}

pub fn lock_path() -> Result<PathBuf, AgentError> {
    Ok(state_directory()?.join(LOCK_FILE))
}

pub fn log_path() -> Result<PathBuf, AgentError> {
    Ok(state_directory()?.join(LOG_FILE))
}

pub fn executable_stamp(path: &Path) -> Result<u64, AgentError> {
    let metadata = fs::metadata(path)?;
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    Ok(modified ^ metadata.len().rotate_left(17))
}
