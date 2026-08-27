use crate::local_agent::AgentError;
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

const STATE_FILE: &str = "local-agent-v1.json";
const LOCK_FILE: &str = "local-agent-v1.lock";
const LOG_FILE: &str = "local-agent.log";

pub fn state_directory() -> Result<PathBuf, AgentError> {
    let directory = platform_state_directory()
        .unwrap_or_else(|| env::temp_dir().join("ContextCapsule"));
    fs::create_dir_all(&directory)?;
    Ok(directory)
}

fn platform_state_directory() -> Option<PathBuf> {
    // Runtime state deliberately ignores CONTEXT_CAPSULE_DB. The database
    // override belongs to the SQLite service/worker; moving it must not create
    // another Local Agent singleton or make an existing agent undiscoverable.
    #[cfg(windows)]
    {
        return env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|base| base.join("ContextCapsule"));
    }

    #[cfg(target_os = "macos")]
    {
        return env::var_os("HOME").map(PathBuf::from).map(|home| {
            home.join("Library")
                .join("Application Support")
                .join("ContextCapsule")
        });
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        if let Some(base) = env::var_os("XDG_DATA_HOME") {
            return Some(PathBuf::from(base).join("context-capsule"));
        }
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".local").join("share").join("context-capsule"))
    }
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
