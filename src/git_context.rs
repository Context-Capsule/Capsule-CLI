use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const GIT_REPOSITORIES_SCHEMA_VERSION: u32 = 1;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const GIT_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_CANDIDATES: usize = 256;
const MAX_REPOSITORIES: usize = 128;

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum GitRepositoryEnvironment {
    #[default]
    Local,
    Wsl {
        #[serde(skip_serializing_if = "Option::is_none")]
        distro: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedGitRepository {
    pub repository_root: String,
    pub branch: String,
    #[serde(default)]
    pub environment: GitRepositoryEnvironment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRepositoriesSnapshot {
    pub schema_version: u32,
    pub repositories: Vec<SavedGitRepository>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitRestoreReport {
    pub repositories_total: usize,
    pub already_on_branch: usize,
    pub planned_checkouts: usize,
    pub checked_out: usize,
    pub skipped_dirty: usize,
    pub skipped_missing_branch: usize,
    pub unavailable: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GitLocation {
    path: String,
    environment: GitRepositoryEnvironment,
}

/// Adds the branch-only Git context for repositories referenced by the saved
/// VS Code workspace or by terminal working directories. This intentionally
/// does not store commit IDs or file diffs.
pub fn capture_into_snapshot(snapshot: &mut Value) {
    if snapshot.get("git_repositories").is_some() {
        return;
    }

    let mut repositories = Vec::new();
    let mut seen_repositories = HashSet::new();
    for candidate in collect_candidates(snapshot).into_iter().take(MAX_CANDIDATES) {
        let Some(repository) = probe_repository(&candidate) else {
            continue;
        };
        let key = repository_key(&repository);
        if seen_repositories.insert(key) {
            repositories.push(repository);
            if repositories.len() >= MAX_REPOSITORIES {
                break;
            }
        }
    }

    if repositories.is_empty() {
        return;
    }

    let section = GitRepositoriesSnapshot {
        schema_version: GIT_REPOSITORIES_SCHEMA_VERSION,
        repositories,
    };
    let Ok(value) = serde_json::to_value(section) else {
        return;
    };
    if let Some(object) = snapshot.as_object_mut() {
        object.insert("git_repositories".to_owned(), value);
    }
}

/// Restores only the saved branch. A repository is never stashed, committed,
/// reset, cleaned, or otherwise mutated when its working tree has changes.
/// Missing branches are also left untouched.
pub fn restore_from_snapshot(snapshot: &Value, dry_run: bool) -> GitRestoreReport {
    let Some(value) = snapshot.get("git_repositories") else {
        return GitRestoreReport::default();
    };

    let section: GitRepositoriesSnapshot = match serde_json::from_value(value.clone()) {
        Ok(section) => section,
        Err(error) => {
            return GitRestoreReport {
                warnings: vec![format!(
                    "Git branch restore metadata is invalid and was skipped: {error}"
                )],
                ..GitRestoreReport::default()
            };
        }
    };

    if section.schema_version != GIT_REPOSITORIES_SCHEMA_VERSION {
        return GitRestoreReport {
            warnings: vec![format!(
                "Git branch restore metadata uses unsupported schema {}; expected {}",
                section.schema_version, GIT_REPOSITORIES_SCHEMA_VERSION
            )],
            ..GitRestoreReport::default()
        };
    }

    restore_repositories(&section.repositories, dry_run, true)
}

fn collect_candidates(snapshot: &Value) -> Vec<GitLocation> {
    let mut candidates = Vec::new();

    if let Some(vscode) = snapshot.pointer("/editors/vscode").filter(|value| !value.is_null()) {
        let inferred_wsl_distro = vscode_wsl_distro(vscode);
        if let Some(folders) = vscode.get("workspaceFolders").and_then(Value::as_array) {
            for folder in folders {
                if let Some(uri) = folder.get("uri").and_then(Value::as_str)
                    && let Some(location) = location_from_uri(uri)
                {
                    candidates.push(location);
                }
            }
        }

        let remote_name = vscode.get("remoteName").and_then(Value::as_str);
        if let Some(terminals) = vscode.get("integratedTerminals").and_then(Value::as_array) {
            for terminal in terminals {
                let Some(cwd) = terminal.get("cwd").and_then(Value::as_str) else {
                    continue;
                };
                let cwd_is_uri = terminal
                    .get("cwdIsUri")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if cwd_is_uri || cwd.contains("://") {
                    if let Some(location) = location_from_uri(cwd) {
                        candidates.push(location);
                    }
                    continue;
                }

                match remote_name {
                    Some("wsl") => candidates.push(GitLocation {
                        path: cwd.to_owned(),
                        environment: GitRepositoryEnvironment::Wsl {
                            distro: inferred_wsl_distro.clone(),
                        },
                    }),
                    Some(_) => {
                        // Remote SSH/dev-container paths are not local filesystem
                        // locations and cannot be safely queried by the local CLI.
                    }
                    None => candidates.push(location_from_plain_path(cwd)),
                }
            }
        }
    }

    if let Some(sessions) = snapshot.pointer("/terminals/sessions").and_then(Value::as_array) {
        for session in sessions {
            let Some(cwd) = session.get("working_directory").and_then(Value::as_str) else {
                continue;
            };
            let environment = session.get("environment");
            let kind = environment
                .and_then(|value| value.get("kind"))
                .and_then(Value::as_str)
                .unwrap_or("windows");
            if kind == "wsl" {
                let configured_distro = environment
                    .and_then(|value| value.get("distro"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some((unc_distro, path)) = parse_wsl_unc(cwd) {
                    candidates.push(GitLocation {
                        path,
                        environment: GitRepositoryEnvironment::Wsl {
                            distro: Some(unc_distro),
                        },
                    });
                } else {
                    candidates.push(GitLocation {
                        path: cwd.to_owned(),
                        environment: GitRepositoryEnvironment::Wsl {
                            distro: configured_distro,
                        },
                    });
                }
            } else {
                candidates.push(location_from_plain_path(cwd));
            }
        }
    }

    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|candidate| seen.insert(location_key(candidate)))
        .collect()
}

fn vscode_wsl_distro(vscode: &Value) -> Option<String> {
    vscode
        .get("workspaceFolders")
        .and_then(Value::as_array)
        .and_then(|folders| {
            folders.iter().find_map(|folder| {
                folder
                    .get("uri")
                    .and_then(Value::as_str)
                    .and_then(parse_wsl_remote_uri)
                    .map(|(distro, _)| distro)
            })
        })
}

fn location_from_uri(uri: &str) -> Option<GitLocation> {
    if let Some(path) = file_uri_to_path(uri) {
        return Some(location_from_plain_path(&path));
    }
    parse_wsl_remote_uri(uri).map(|(distro, path)| GitLocation {
        path,
        environment: GitRepositoryEnvironment::Wsl {
            distro: Some(distro),
        },
    })
}

fn location_from_plain_path(path: &str) -> GitLocation {
    if let Some((distro, wsl_path)) = parse_wsl_unc(path) {
        GitLocation {
            path: wsl_path,
            environment: GitRepositoryEnvironment::Wsl {
                distro: Some(distro),
            },
        }
    } else {
        GitLocation {
            path: path.to_owned(),
            environment: GitRepositoryEnvironment::Local,
        }
    }
}

fn file_uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    if rest.starts_with('/') {
        let mut path = percent_decode(rest);
        #[cfg(windows)]
        if path.len() >= 3 && path.as_bytes()[0] == b'/' && path.as_bytes()[2] == b':' {
            path.remove(0);
        }
        Some(path)
    } else {
        let decoded = percent_decode(rest);
        #[cfg(windows)]
        {
            return Some(format!(r"\\{}", decoded.replace('/', r"\")));
        }
        #[cfg(not(windows))]
        {
            Some(format!("//{decoded}"))
        }
    }
}

fn parse_wsl_remote_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("vscode-remote://wsl+")?;
    let (authority, path) = rest.split_once('/')?;
    let distro = percent_decode(authority);
    if distro.trim().is_empty() {
        return None;
    }
    Some((distro, format!("/{}", percent_decode(path))))
}

fn parse_wsl_unc(value: &str) -> Option<(String, String)> {
    let normalized = value.replace('\\', "/");
    let without_slashes = normalized.trim_start_matches('/');
    let lower = without_slashes.to_ascii_lowercase();
    let prefix_len = if lower.starts_with("wsl.localhost/") {
        "wsl.localhost/".len()
    } else if lower.starts_with("wsl$/") {
        "wsl$/".len()
    } else {
        return None;
    };
    let tail = &without_slashes[prefix_len..];
    let (distro, path) = tail.split_once('/').unwrap_or((tail, ""));
    if distro.is_empty() {
        return None;
    }
    let path = if path.is_empty() {
        "/".to_owned()
    } else {
        format!("/{path}")
    };
    Some((distro.to_owned(), path))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            output.push((high << 4) | low);
            index += 3;
            continue;
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn probe_repository(candidate: &GitLocation) -> Option<SavedGitRepository> {
    let root = git_text(candidate, &["rev-parse", "--show-toplevel"]).ok()?;
    let branch = git_text(candidate, &["branch", "--show-current"]).ok()?;
    let branch = branch.trim();
    if branch.is_empty() {
        return None;
    }

    Some(SavedGitRepository {
        repository_root: root.trim().to_owned(),
        branch: branch.to_owned(),
        environment: candidate.environment.clone(),
    })
}

fn restore_repositories(
    repositories: &[SavedGitRepository],
    dry_run: bool,
    clear_after_checkout: bool,
) -> GitRestoreReport {
    let mut report = GitRestoreReport {
        repositories_total: repositories.len(),
        ..GitRestoreReport::default()
    };
    let mut seen = HashSet::new();

    for saved in repositories.iter().take(MAX_REPOSITORIES) {
        if saved.branch.trim().is_empty() || saved.repository_root.trim().is_empty() {
            report.unavailable += 1;
            report
                .warnings
                .push("Git branch restore skipped an entry with an empty path or branch".to_owned());
            continue;
        }
        if !seen.insert(repository_key(saved)) {
            continue;
        }

        let location = GitLocation {
            path: saved.repository_root.clone(),
            environment: saved.environment.clone(),
        };
        let current = match git_text(&location, &["branch", "--show-current"]) {
            Ok(branch) => branch.trim().to_owned(),
            Err(error) => {
                report.unavailable += 1;
                report.warnings.push(format!(
                    "Git branch restore skipped '{}': repository is unavailable ({error})",
                    saved.repository_root
                ));
                continue;
            }
        };
        if current == saved.branch {
            report.already_on_branch += 1;
            continue;
        }

        let status = match git_text(&location, &["status", "--porcelain", "--untracked-files=normal"]) {
            Ok(status) => status,
            Err(error) => {
                report.unavailable += 1;
                report.warnings.push(format!(
                    "Git branch restore skipped '{}': could not verify working-tree safety ({error})",
                    saved.repository_root
                ));
                continue;
            }
        };
        if !status.trim().is_empty() {
            report.skipped_dirty += 1;
            report.warnings.push(format!(
                "Git branch restore left '{}' on branch '{}': uncommitted/staged/untracked changes are present; saved branch '{}' was not checked out",
                saved.repository_root,
                if current.is_empty() { "(detached HEAD)" } else { &current },
                saved.branch
            ));
            continue;
        }

        match local_branch_exists(&location, &saved.branch) {
            Ok(true) => {}
            Ok(false) => {
                report.skipped_missing_branch += 1;
                report.warnings.push(format!(
                    "Git branch restore left '{}' unchanged because saved branch '{}' no longer exists locally",
                    saved.repository_root, saved.branch
                ));
                continue;
            }
            Err(error) => {
                report.unavailable += 1;
                report.warnings.push(format!(
                    "Git branch restore skipped '{}': could not verify saved branch '{}' ({error})",
                    saved.repository_root, saved.branch
                ));
                continue;
            }
        }

        if dry_run {
            report.planned_checkouts += 1;
            report.warnings.push(format!(
                "Git restore plan: would checkout '{}' in '{}' because the working tree is clean",
                saved.branch, saved.repository_root
            ));
            continue;
        }

        match checkout_branch(&location, &saved.branch) {
            Ok(()) => {
                report.checked_out += 1;
                crate::logging::info(
                    "git",
                    format!("restored saved Git branch; branch={}", saved.branch),
                );
                if clear_after_checkout
                    && let Err(error) = clear_terminal_after_checkout(&saved.environment)
                {
                    report.warnings.push(format!(
                        "Git branch '{}' was restored for '{}', but the requested clear command failed: {error}",
                        saved.branch, saved.repository_root
                    ));
                }
            }
            Err(error) => {
                report.unavailable += 1;
                report.warnings.push(format!(
                    "Git branch restore could not checkout '{}' in '{}': {error}",
                    saved.branch, saved.repository_root
                ));
            }
        }
    }

    report
}

fn local_branch_exists(location: &GitLocation, branch: &str) -> Result<bool, String> {
    let reference = format!("refs/heads/{branch}");
    let output = git_output_owned(location, &["show-ref", "--verify", "--quiet", &reference])?;
    if output.status.success() {
        Ok(true)
    } else if output.status.code() == Some(1) {
        Ok(false)
    } else {
        Err(command_error(&output))
    }
}

fn checkout_branch(location: &GitLocation, branch: &str) -> Result<(), String> {
    let output = git_output_owned(location, &["checkout", "--quiet", branch])?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error(&output))
    }
}

fn git_text(location: &GitLocation, args: &[&str]) -> Result<String, String> {
    let output = git_output_owned(location, args)?;
    if !output.status.success() {
        return Err(command_error(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_output_owned(location: &GitLocation, args: &[&str]) -> Result<Output, String> {
    let mut command = git_command(location).map_err(|error| error.to_string())?;
    command
        .arg("-C")
        .arg(&location.path)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    output_with_timeout(command, GIT_COMMAND_TIMEOUT).map_err(|error| error.to_string())
}

fn git_command(location: &GitLocation) -> io::Result<Command> {
    match &location.environment {
        GitRepositoryEnvironment::Local => Ok(Command::new("git")),
        GitRepositoryEnvironment::Wsl { distro } => {
            #[cfg(windows)]
            {
                let mut command = Command::new("wsl.exe");
                if let Some(distro) = distro.as_deref().filter(|value| !value.trim().is_empty()) {
                    command.arg("-d").arg(distro);
                }
                command.arg("--").arg("git");
                Ok(command)
            }
            #[cfg(not(windows))]
            {
                let _ = distro;
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "WSL Git context can only be restored on Windows",
                ))
            }
        }
    }
}

fn output_with_timeout(mut command: Command, timeout: Duration) -> io::Result<Output> {
    let mut child = command.spawn()?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Git command timed out",
            ));
        }
        thread::sleep(GIT_COMMAND_POLL_INTERVAL);
    }
}

fn command_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("Git exited with status {}", output.status)
    } else {
        stderr.to_owned()
    }
}

fn clear_terminal_after_checkout(environment: &GitRepositoryEnvironment) -> Result<(), String> {
    let status = match environment {
        GitRepositoryEnvironment::Local => {
            #[cfg(windows)]
            {
                Command::new("cmd.exe")
                    .args(["/C", "cls"])
                    .status()
                    .map_err(|error| error.to_string())?
            }
            #[cfg(not(windows))]
            {
                Command::new("clear")
                    .status()
                    .map_err(|error| error.to_string())?
            }
        }
        GitRepositoryEnvironment::Wsl { distro } => {
            #[cfg(windows)]
            {
                let mut command = Command::new("wsl.exe");
                if let Some(distro) = distro.as_deref().filter(|value| !value.trim().is_empty()) {
                    command.arg("-d").arg(distro);
                }
                command
                    .arg("--")
                    .arg("clear")
                    .status()
                    .map_err(|error| error.to_string())?
            }
            #[cfg(not(windows))]
            {
                let _ = distro;
                return Err("WSL clear command is unavailable on this platform".to_owned());
            }
        }
    };
    if status.success() {
        Ok(())
    } else {
        Err(format!("clear command exited with status {status}"))
    }
}

fn location_key(location: &GitLocation) -> String {
    let mut path = location.path.trim_end_matches(['/', '\\']).replace('\\', "/");
    #[cfg(windows)]
    if matches!(location.environment, GitRepositoryEnvironment::Local) {
        path.make_ascii_lowercase();
    }
    match &location.environment {
        GitRepositoryEnvironment::Local => format!("local:{path}"),
        GitRepositoryEnvironment::Wsl { distro } => format!(
            "wsl:{}:{path}",
            distro.as_deref().unwrap_or_default().to_ascii_lowercase()
        ),
    }
}

fn repository_key(repository: &SavedGitRepository) -> String {
    location_key(&GitLocation {
        path: repository.repository_root.clone(),
        environment: repository.environment.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "context-capsule-git-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn run(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn current_branch(repo: &Path) -> String {
        String::from_utf8_lossy(
            &Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["branch", "--show-current"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_owned()
    }

    fn make_repo(label: &str) -> Option<PathBuf> {
        if !git_available() {
            return None;
        }
        let repo = unique_temp_dir(label);
        fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init"]);
        run(&repo, &["config", "user.email", "context-capsule@example.invalid"]);
        run(&repo, &["config", "user.name", "Context Capsule Tests"]);
        run(&repo, &["checkout", "-b", "base"]);
        fs::write(repo.join("tracked.txt"), "initial\n").unwrap();
        run(&repo, &["add", "tracked.txt"]);
        run(&repo, &["commit", "-m", "initial"]);
        run(&repo, &["checkout", "-b", "saved-branch"]);
        Some(repo)
    }

    fn local_saved(repo: &Path, branch: &str) -> SavedGitRepository {
        SavedGitRepository {
            repository_root: repo.to_string_lossy().to_string(),
            branch: branch.to_owned(),
            environment: GitRepositoryEnvironment::Local,
        }
    }

    #[test]
    fn parses_file_and_wsl_workspace_locations() {
        #[cfg(windows)]
        assert_eq!(
            file_uri_to_path("file:///C:/work/My%20Repo").as_deref(),
            Some("C:/work/My Repo")
        );
        #[cfg(not(windows))]
        assert_eq!(
            file_uri_to_path("file:///tmp/My%20Repo").as_deref(),
            Some("/tmp/My Repo")
        );
        assert_eq!(
            parse_wsl_remote_uri("vscode-remote://wsl+Ubuntu/home/dhia/project"),
            Some(("Ubuntu".to_owned(), "/home/dhia/project".to_owned()))
        );
        assert_eq!(
            parse_wsl_unc(r"\\wsl.localhost\Ubuntu\home\dhia\project"),
            Some(("Ubuntu".to_owned(), "/home/dhia/project".to_owned()))
        );
    }

    #[test]
    fn candidate_collection_includes_vscode_and_terminal_cwds_without_duplicates() {
        let snapshot = json!({
            "editors": { "vscode": {
                "workspaceFolders": [
                    { "uri": "file:///C:/work/project", "name": "project", "index": 0 },
                    { "uri": "vscode-remote://wsl+Ubuntu/home/dhia/api", "name": "api", "index": 1 }
                ],
                "integratedTerminals": [
                    { "cwd": "C:/work/project", "cwdIsUri": false },
                    { "cwd": "/home/dhia/worker", "cwdIsUri": false }
                ],
                "remoteName": "wsl"
            }},
            "terminals": { "sessions": [
                { "working_directory": "C:/work/project", "environment": { "kind": "windows" } },
                { "working_directory": "//wsl.localhost/Ubuntu/home/dhia/api", "environment": { "kind": "wsl", "distro": "Ubuntu" } }
            ]}
        });
        let candidates = collect_candidates(&snapshot);
        assert!(candidates.iter().any(|candidate| candidate.path.ends_with("work/project")));
        assert!(candidates.iter().any(|candidate| {
            candidate.path == "/home/dhia/api"
                && candidate.environment
                    == GitRepositoryEnvironment::Wsl {
                        distro: Some("Ubuntu".to_owned()),
                    }
        }));
        assert!(candidates.iter().any(|candidate| candidate.path == "/home/dhia/worker"));
    }

    #[test]
    fn capture_records_branch_only_and_deduplicates_repo_roots() {
        let Some(repo) = make_repo("capture") else {
            return;
        };
        let nested = repo.join("nested");
        fs::create_dir_all(&nested).unwrap();
        #[cfg(windows)]
        let uri = format!("file:///{}", repo.to_string_lossy().replace('\\', "/"));
        #[cfg(not(windows))]
        let uri = format!("file://{}", repo.to_string_lossy());
        let mut snapshot = json!({
            "editors": { "vscode": {
                "workspaceFolders": [{ "uri": uri, "name": "repo", "index": 0 }],
                "integratedTerminals": [],
                "remoteName": null
            }},
            "terminals": { "sessions": [
                { "working_directory": nested.to_string_lossy(), "environment": { "kind": "windows" } }
            ]}
        });
        capture_into_snapshot(&mut snapshot);
        let section = snapshot.get("git_repositories").expect("git section");
        let repositories = section.get("repositories").unwrap().as_array().unwrap();
        assert_eq!(repositories.len(), 1);
        assert_eq!(repositories[0]["branch"], "saved-branch");
        assert!(repositories[0].get("head").is_none());
        assert!(repositories[0].get("commit").is_none());
        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn clean_repo_checks_out_saved_branch() {
        let Some(repo) = make_repo("restore-clean") else {
            return;
        };
        run(&repo, &["checkout", "base"]);
        let report = restore_repositories(&[local_saved(&repo, "saved-branch")], false, false);
        assert_eq!(report.checked_out, 1);
        assert_eq!(current_branch(&repo), "saved-branch");
        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn dirty_repo_is_never_switched() {
        let Some(repo) = make_repo("restore-dirty") else {
            return;
        };
        run(&repo, &["checkout", "base"]);
        fs::write(repo.join("tracked.txt"), "changed\n").unwrap();
        let report = restore_repositories(&[local_saved(&repo, "saved-branch")], false, false);
        assert_eq!(report.skipped_dirty, 1);
        assert_eq!(report.checked_out, 0);
        assert_eq!(current_branch(&repo), "base");
        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn missing_saved_branch_is_never_recreated() {
        let Some(repo) = make_repo("restore-missing") else {
            return;
        };
        run(&repo, &["checkout", "base"]);
        run(&repo, &["branch", "-D", "saved-branch"]);
        let report = restore_repositories(&[local_saved(&repo, "saved-branch")], false, false);
        assert_eq!(report.skipped_missing_branch, 1);
        assert_eq!(report.checked_out, 0);
        assert_eq!(current_branch(&repo), "base");
        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn dry_run_plans_checkout_without_mutating_repo() {
        let Some(repo) = make_repo("restore-dry-run") else {
            return;
        };
        run(&repo, &["checkout", "base"]);
        let report = restore_repositories(&[local_saved(&repo, "saved-branch")], true, false);
        assert_eq!(report.planned_checkouts, 1);
        assert_eq!(report.checked_out, 0);
        assert_eq!(current_branch(&repo), "base");
        let _ = fs::remove_dir_all(repo);
    }
}
