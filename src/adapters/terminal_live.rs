#[path = "terminal.rs"]
mod base;

pub use base::{
    RestartPlan, ShellKind, TerminalEnvironment, TerminalHistoryPolicy, TerminalHost,
    TerminalLayoutAction, TerminalSession, TerminalSnapshot, TerminalSource, TerminalStatus,
    TerminalWindowSize, WindowsTerminalLayout, WorkingDirectorySource,
};

/// Keep the mature terminal/process adapter authoritative, then make one narrow
/// best-effort correction for active external PowerShell services: Win32 process
/// metadata contains the resolved executable path, while PSReadLine normally
/// stores the command text the user submitted. We only substitute a history
/// entry when its executable identity and every argument exactly match the live
/// child process. Otherwise the resolved process command is kept unchanged.
pub fn discover() -> TerminalSnapshot {
    let mut snapshot = base::discover();

    #[cfg(windows)]
    recover_powershell_aliases(&mut snapshot);

    snapshot
}

#[cfg(windows)]
use std::{
    env,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

const MAX_RECOVERED_COMMAND_CHARS: usize = 2048;
#[cfg(windows)]
const MAX_HISTORY_BYTES: u64 = 256 * 1024;
#[cfg(windows)]
const MAX_HISTORY_ENTRIES: usize = 128;

#[cfg(windows)]
fn recover_powershell_aliases(snapshot: &mut TerminalSnapshot) {
    if !snapshot.sessions.iter().any(is_recoverable_powershell_service) {
        return;
    }
    let Some(history) = recent_psreadline_history() else {
        return;
    };

    for session in &mut snapshot.sessions {
        if !is_recoverable_powershell_service(session) {
            continue;
        }
        let Some(resolved) = session.foreground_command.clone() else {
            continue;
        };
        if let Some(original) = recover_matching_history_command(&resolved, &history) {
            session.foreground_command = Some(original);
        }
    }
}

#[cfg(windows)]
fn is_recoverable_powershell_service(session: &TerminalSession) -> bool {
    matches!(&session.environment, TerminalEnvironment::Windows)
        && matches!(&session.shell, ShellKind::PowerShell | ShellKind::WindowsPowerShell)
        && !matches!(&session.host, TerminalHost::VisualStudioCode | TerminalHost::Cursor)
        && session.foreground_command.is_some()
}

#[cfg(windows)]
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

/// Read only a bounded tail of the default PSReadLine history file. This is a
/// passive file read: it never starts PowerShell, attaches to a console, sends
/// input, or queries a live runspace. If the user configured a custom history
/// path/style, recovery simply falls back to the resolved process command.
#[cfg(windows)]
fn recent_psreadline_history() -> Option<Vec<String>> {
    let path = default_psreadline_history_path()?;
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(MAX_HISTORY_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;

    let mut bytes = Vec::with_capacity((length - start).min(MAX_HISTORY_BYTES) as usize);
    let mut limited = file.take(MAX_HISTORY_BYTES);
    limited.read_to_end(&mut bytes).ok()?;
    if bytes.is_empty() {
        return None;
    }

    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    // A bounded tail may begin in the middle of an old history entry. Discard
    // that first fragment rather than treating it as submitted command text.
    if start > 0 {
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandToken {
    value: String,
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
        && !value.contains('\\')
        && !value.contains('/')
        && !value.contains(':')
        && !value.starts_with('.')
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
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

/// Walk newest-to-oldest because PSReadLine history is shared by ConsoleHost
/// sessions. Unrelated newer commands (including the `capsule save` command
/// itself) are ignored. Once we find the newest command structurally equivalent
/// to the running child, we only use it when it began with a simple executable
/// token such as `python`, `node`, or `npm`. If the newest equivalent history
/// entry used an absolute/relative path or other shell syntax, we stop and keep
/// the resolved process command rather than reaching farther back for a prettier
/// but potentially wrong spelling.
fn recover_matching_history_command(resolved: &str, history: &[String]) -> Option<String> {
    let resolved_tokens = tokenize_command_line(resolved);
    let resolved_first = resolved_tokens.first()?;

    for entry in history.iter().rev() {
        let candidate = entry.trim();
        if candidate.is_empty()
            || candidate.chars().count() > MAX_RECOVERED_COMMAND_CHARS
            || candidate.chars().any(char::is_control)
            || candidate.contains(';')
            || candidate.contains('|')
            || candidate.contains("&&")
            || candidate.contains("||")
        {
            continue;
        }

        let candidate_tokens = tokenize_command_line(candidate);
        if !structurally_matches(&candidate_tokens, &resolved_tokens) {
            continue;
        }

        // This is the newest equivalent submitted command. If it was not a
        // simple alias/name, do not skip it and accidentally pick an older alias.
        if !is_bare_executable_token(&candidate_tokens[0].value) {
            return None;
        }
        if executable_key(&candidate_tokens[0].value) != executable_key(&resolved_first.value) {
            return None;
        }
        return Some(candidate.to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venv_python_process_recovers_typed_python_alias() {
        let resolved = r#""C:\Users\monji\OneDrive\Bureau\P\SW\Dino-Game-Auto-Player\venv\Scripts\python.exe" -m app"#;
        let history = vec![
            "python -m app".to_owned(),
            "cargo run -- save test --cli-force".to_owned(),
        ];
        assert_eq!(
            recover_matching_history_command(resolved, &history).as_deref(),
            Some("python -m app")
        );
    }

    #[test]
    fn unrelated_newer_history_entries_are_ignored() {
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
    fn newest_equivalent_absolute_command_blocks_older_alias_rewrite() {
        let resolved = r#""C:\venv\Scripts\python.exe" -m app"#;
        let history = vec![
            "python -m app".to_owned(),
            r#""C:\venv\Scripts\python.exe" -m app"#.to_owned(),
            "cargo run -- save demo --cli-force".to_owned(),
        ];
        assert_eq!(recover_matching_history_command(resolved, &history), None);
    }

    #[test]
    fn argument_mismatch_never_rewrites_process_command() {
        let resolved = r#""C:\venv\Scripts\python.exe" -m app"#;
        let history = vec!["python -m other".to_owned()];
        assert_eq!(recover_matching_history_command(resolved, &history), None);
    }

    #[test]
    fn executable_identity_mismatch_never_rewrites_process_command() {
        let resolved = r#""C:\venv\Scripts\python.exe" -m app"#;
        let history = vec!["py -m app".to_owned()];
        assert_eq!(recover_matching_history_command(resolved, &history), None);
    }

    #[test]
    fn compound_history_command_is_not_reduced_to_child_process() {
        let resolved = r#""C:\venv\Scripts\python.exe" -m app"#;
        let history = vec!["$env:MODE='prod'; python -m app".to_owned()];
        assert_eq!(recover_matching_history_command(resolved, &history), None);
    }

    #[test]
    fn path_like_candidate_is_not_rewritten_to_an_alias() {
        let resolved = r#""C:\Tools\python.exe" -m app"#;
        let history = vec![r#""C:\Tools\python.exe" -m app"#.to_owned()];
        assert_eq!(recover_matching_history_command(resolved, &history), None);
    }

    #[test]
    fn quoted_argument_text_is_preserved_when_values_match() {
        let resolved = r#""C:\Tools\python.exe" "hello world.py""#;
        let history = vec![r#"python "hello world.py""#.to_owned()];
        assert_eq!(
            recover_matching_history_command(resolved, &history).as_deref(),
            Some(r#"python "hello world.py""#)
        );
    }

    #[test]
    fn service_command_with_shell_operator_is_never_recovered() {
        let resolved = r#""C:\Tools\node.exe" server.js"#;
        let history = vec!["node server.js | tee log.txt".to_owned()];
        assert_eq!(recover_matching_history_command(resolved, &history), None);
    }

    #[test]
    fn bare_executable_gate_accepts_expected_cli_names_only() {
        assert!(is_bare_executable_token("python"));
        assert!(is_bare_executable_token("python3.13"));
        assert!(is_bare_executable_token("npm"));
        assert!(is_bare_executable_token("my-server_cli"));
        assert!(!is_bare_executable_token("C:\\Tools\\python.exe"));
        assert!(!is_bare_executable_token(".\\server.exe"));
        assert!(!is_bare_executable_token("&"));
    }
}
