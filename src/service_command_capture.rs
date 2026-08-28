use std::{
    env,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

const MAX_RECOVERED_COMMAND_CHARS: usize = 2048;
const MAX_HISTORY_BYTES: u64 = 256 * 1024;
const MAX_HISTORY_ENTRIES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandToken {
    value: String,
}

/// Best-effort recovery of the command text submitted to an external PowerShell
/// session. This is deliberately passive: it reads only the bounded tail of the
/// default PSReadLine history file. It never starts PowerShell, connects to a
/// runspace, attaches to a console, or sends input.
///
/// The returned spelling is used only for display/prompt metadata. The resolved
/// Win32 child-process command remains the authoritative execution command.
pub(super) fn recover_powershell_typed_command(resolved: &str) -> Option<String> {
    let history = recent_psreadline_history()?;
    recover_matching_history_command(resolved, &history)
}

pub(super) fn commands_equivalent(left: &str, right: &str) -> bool {
    let left = tokenize_command_line(left);
    let right = tokenize_command_line(right);
    structurally_matches(&left, &right)
}

fn default_psreadline_history_path() -> Option<PathBuf> {
    let appdata = env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("PowerShell")
            .join("PSReadLine")
            .join("ConsoleHost_history.txt"),
    )
}

fn recent_psreadline_history() -> Option<Vec<String>> {
    let path = default_psreadline_history_path()?;
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(MAX_HISTORY_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;

    let mut bytes = Vec::with_capacity((length - start).min(MAX_HISTORY_BYTES) as usize);
    file.take(MAX_HISTORY_BYTES).read_to_end(&mut bytes).ok()?;
    if bytes.is_empty() {
        return None;
    }

    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    if start > 0 {
        // A bounded tail can begin in the middle of an old entry. Never treat
        // that fragment as submitted command text.
        let _ = lines.next();
    }

    let mut recent = lines
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.contains('\u{fffd}'))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if recent.len() > MAX_HISTORY_ENTRIES {
        recent.drain(..recent.len() - MAX_HISTORY_ENTRIES);
    }
    (!recent.is_empty()).then_some(recent)
}

fn recover_matching_history_command(resolved: &str, history: &[String]) -> Option<String> {
    let resolved_tokens = tokenize_command_line(resolved);
    resolved_tokens.first()?;

    for entry in history.iter().rev() {
        let candidate = entry.trim();
        if candidate.is_empty()
            || candidate.chars().count() > MAX_RECOVERED_COMMAND_CHARS
            || candidate.chars().any(char::is_control)
            || contains_shell_operator(candidate)
        {
            continue;
        }

        let candidate_tokens = tokenize_command_line(candidate);
        if !structurally_matches(&candidate_tokens, &resolved_tokens) {
            continue;
        }

        // The newest equivalent submitted command is authoritative. If it used
        // a path/shell expression, do not skip it and accidentally choose an
        // older prettier alias that may describe a different shell context.
        if !is_bare_executable_token(&candidate_tokens[0].value) {
            return None;
        }
        return Some(candidate.to_owned());
    }
    None
}

fn contains_shell_operator(value: &str) -> bool {
    value.contains(';') || value.contains('|') || value.contains("&&") || value.contains("||")
}

fn tokenize_command_line(text: &str) -> Vec<CommandToken> {
    let mut tokens = Vec::new();
    let mut value = String::new();
    let mut started = false;
    let mut quote = None;

    for ch in text.chars() {
        if quote.is_none() && ch.is_whitespace() {
            if started {
                tokens.push(CommandToken {
                    value: std::mem::take(&mut value),
                });
                started = false;
            }
            continue;
        }

        started = true;
        match quote {
            Some(active) if ch == active => quote = None,
            Some(_) => value.push(ch),
            None if matches!(ch, '\'' | '"') => quote = Some(ch),
            None => value.push(ch),
        }
    }

    if started {
        tokens.push(CommandToken { value });
    }
    tokens
}

fn structurally_matches(candidate: &[CommandToken], resolved: &[CommandToken]) -> bool {
    candidate.len() == resolved.len()
        && !candidate.is_empty()
        && executable_key(&candidate[0].value) == executable_key(&resolved[0].value)
        && candidate[1..]
            .iter()
            .zip(&resolved[1..])
            .all(|(left, right)| left.value == right.value)
}

fn executable_key(value: &str) -> String {
    let leaf = value
        .rsplit(['\\', '/'])
        .find(|part| !part.is_empty())
        .unwrap_or(value)
        .to_ascii_lowercase();
    leaf.strip_suffix(".exe").unwrap_or(&leaf).to_owned()
}

fn is_bare_executable_token(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['\\', '/', ':'])
        && !value.starts_with('.')
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venv_python_process_recovers_typed_python_without_changing_execution_identity() {
        let resolved = r#""C:\Users\monji\OneDrive\Bureau\P\SW\Dino-Game-Auto-Player\venv\Scripts\python.exe" -m app"#;
        let history = vec![
            "python -m app".to_owned(),
            "cargo run -- save test --cli-force".to_owned(),
        ];
        assert_eq!(
            recover_matching_history_command(resolved, &history).as_deref(),
            Some("python -m app")
        );
        assert!(commands_equivalent("python -m app", resolved));
    }

    #[test]
    fn unrelated_newer_history_does_not_hide_current_service() {
        let resolved = r#""C:\venv\Scripts\python.exe" -m app"#;
        let history = vec![
            "python -m app".to_owned(),
            "git status".to_owned(),
            "cargo run -- save demo --cli-force".to_owned(),
        ];
        assert_eq!(
            recover_matching_history_command(resolved, &history).as_deref(),
            Some("python -m app")
        );
    }

    #[test]
    fn newest_equivalent_path_command_blocks_older_alias() {
        let resolved = r#""C:\venv\Scripts\python.exe" -m app"#;
        let history = vec![
            "python -m app".to_owned(),
            resolved.to_owned(),
            "cargo run -- save demo --cli-force".to_owned(),
        ];
        assert_eq!(recover_matching_history_command(resolved, &history), None);
    }

    #[test]
    fn mismatched_or_compound_commands_are_never_substituted() {
        let resolved = r#""C:\venv\Scripts\python.exe" -m app"#;
        assert_eq!(
            recover_matching_history_command(resolved, &["python -m other".to_owned()]),
            None
        );
        assert_eq!(
            recover_matching_history_command(
                resolved,
                &["$env:MODE='prod'; python -m app".to_owned()]
            ),
            None
        );
        assert_eq!(
            recover_matching_history_command(
                resolved,
                &["Get-Content x | python -m app".to_owned()]
            ),
            None
        );
    }

    #[test]
    fn quoted_arguments_compare_by_value_and_preserve_typed_spelling() {
        let resolved = r#""C:\Tools\python.exe" "hello world.py""#;
        let history = vec![r#"python "hello world.py""#.to_owned()];
        assert_eq!(
            recover_matching_history_command(resolved, &history).as_deref(),
            Some(r#"python "hello world.py""#)
        );
    }

    #[test]
    fn executable_identity_is_case_insensitive_and_ignores_exe_suffix() {
        assert!(commands_equivalent(
            "PYTHON -m app",
            r#""C:\venv\Scripts\python.exe" -m app"#
        ));
        assert!(!commands_equivalent(
            "py -m app",
            r#""C:\venv\Scripts\python.exe" -m app"#
        ));
    }

    #[test]
    fn bare_executable_gate_rejects_paths_and_shell_tokens() {
        assert!(is_bare_executable_token("python"));
        assert!(is_bare_executable_token("python.exe"));
        assert!(is_bare_executable_token("npm"));
        assert!(!is_bare_executable_token("C:\\Tools\\python.exe"));
        assert!(!is_bare_executable_token(".\\server.exe"));
        assert!(!is_bare_executable_token("&"));
    }
}
