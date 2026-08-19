use std::{
    env, fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const DEFAULT_MAX_LOG_BYTES: u64 = 1024 * 1024;
const MAX_MESSAGE_CHARS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }
}

pub fn log_directory() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("CONTEXT_CAPSULE_LOG_DIR") {
        if path.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CONTEXT_CAPSULE_LOG_DIR is empty",
            ));
        }
        return Ok(PathBuf::from(path));
    }

    #[cfg(windows)]
    {
        let base = env::var_os("LOCALAPPDATA").ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is unavailable")
        })?;
        return Ok(PathBuf::from(base).join("ContextCapsule").join("logs"));
    }

    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unavailable"))?;
        return Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("ContextCapsule")
            .join("logs"));
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        if let Some(base) = env::var_os("XDG_STATE_HOME") {
            return Ok(PathBuf::from(base).join("context-capsule").join("logs"));
        }
        let home = env::var_os("HOME")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unavailable"))?;
        Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("context-capsule")
            .join("logs"))
    }
}

pub fn component_log_path(component: &str) -> io::Result<PathBuf> {
    let component = sanitize_component(component)?;
    Ok(log_directory()?.join(format!("{component}.log")))
}

pub fn append(component: &str, level: LogLevel, message: impl AsRef<str>) -> io::Result<PathBuf> {
    let path = component_log_path(component)?;
    append_at(&path, level, message.as_ref(), DEFAULT_MAX_LOG_BYTES)?;
    Ok(path)
}

pub fn info(component: &str, message: impl AsRef<str>) {
    let _ = append(component, LogLevel::Info, message);
}

pub fn warn(component: &str, message: impl AsRef<str>) {
    let _ = append(component, LogLevel::Warn, message);
}

pub fn error(component: &str, message: impl AsRef<str>) {
    let _ = append(component, LogLevel::Error, message);
}

fn append_at(path: &Path, level: LogLevel, message: &str, max_bytes: u64) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    rotate_if_needed(path, max_bytes)?;
    let line = format!(
        "{} [{}] {}\n",
        now_unix_ms(),
        level.as_str(),
        sanitize_message(message)
    );
    use std::io::Write;
    let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

fn rotate_if_needed(path: &Path, max_bytes: u64) -> io::Result<()> {
    let size = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if size < max_bytes {
        return Ok(());
    }

    let rotated = path.with_extension("log.1");
    match fs::remove_file(&rotated) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::rename(path, rotated)?;
    Ok(())
}

fn sanitize_component(component: &str) -> io::Result<String> {
    let trimmed = component.trim();
    if trimmed.is_empty()
        || !trimmed
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "log component must contain only ASCII letters, digits, '-' or '_'",
        ));
    }
    Ok(trimmed.to_ascii_lowercase())
}

fn sanitize_message(message: &str) -> String {
    let mut result = String::with_capacity(message.len().min(MAX_MESSAGE_CHARS));
    for character in message.chars().take(MAX_MESSAGE_CHARS) {
        match character {
            '\r' | '\n' | '\0' => result.push(' '),
            other if other.is_control() => result.push(' '),
            other => result.push(other),
        }
    }
    if message.chars().count() > MAX_MESSAGE_CHARS {
        result.push_str(" …[truncated]");
    }
    result
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "context-capsule-{name}-{}-{}.log",
            std::process::id(),
            now_unix_ms()
        ))
    }

    #[test]
    fn messages_are_single_line_and_bounded() {
        let sanitized = sanitize_message("safe\nsecond\rline\0tail");
        assert_eq!(sanitized, "safe second line tail");
        let oversized = "x".repeat(MAX_MESSAGE_CHARS + 10);
        let sanitized = sanitize_message(&oversized);
        assert!(sanitized.starts_with(&"x".repeat(MAX_MESSAGE_CHARS)));
        assert!(sanitized.ends_with("…[truncated]"));
    }

    #[test]
    fn component_names_cannot_escape_the_log_directory() {
        assert_eq!(sanitize_component("Firefox_2").unwrap(), "firefox_2");
        assert!(sanitize_component("../secret").is_err());
        assert!(sanitize_component("bad/path").is_err());
        assert!(sanitize_component(" ").is_err());
    }

    #[test]
    fn log_rotation_keeps_one_bounded_previous_file() {
        let path = temp_log("rotate");
        fs::write(&path, b"1234567890").unwrap();
        append_at(&path, LogLevel::Info, "new", 10).unwrap();
        let rotated = path.with_extension("log.1");
        assert_eq!(fs::read(&rotated).unwrap(), b"1234567890");
        let current = fs::read_to_string(&path).unwrap();
        assert!(current.contains("[INFO] new"));
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(rotated);
    }
}
