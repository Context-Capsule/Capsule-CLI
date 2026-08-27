#[path = "git_context.rs"]
mod base;

pub use base::{
    GitRepositoriesSnapshot, GitRepositoryEnvironment, GitRestoreReport, SavedGitRepository,
};

use serde_json::Value;
use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use std::ffi::c_void;

const VSCODE_STATE_MAX_AGE: Duration = Duration::from_secs(90);
const MAX_VSCODE_RUNTIME_FILES: usize = 128;
const MAX_VSCODE_GIT_CANDIDATES: usize = 256;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const GIT_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(25);
const GIT_REPOSITORIES_SCHEMA_VERSION: u32 = 1;

#[cfg(windows)]
type Handle = *mut c_void;
#[cfg(windows)]
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> Handle;
    fn CloseHandle(handle: Handle) -> i32;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VsCodeGitLocation {
    path: String,
    environment: GitRepositoryEnvironment,
}

/// Captures the normal serialized terminal/current VS Code context first, then
/// unions branch-only Git context from every recent live VS Code extension
/// host. The normal VS Code semantic snapshot still has one authoritative host;
/// this extra scan exists only so independent open VS Code projects can each
/// contribute their repository branch without changing editor restore routing.
pub fn capture_into_snapshot(snapshot: &mut Value) {
    base::capture_into_snapshot(snapshot);

    let Ok(canonical) = crate::vscode::runtime_state_path() else {
        return;
    };
    augment_from_vscode_runtime_at(snapshot, &canonical, now_unix_ms());
}

pub fn restore_from_snapshot(snapshot: &Value, dry_run: bool) -> GitRestoreReport {
    base::restore_from_snapshot(snapshot, dry_run)
}

fn augment_from_vscode_runtime_at(snapshot: &mut Value, canonical: &Path, now_ms: i64) {
    let mut section = match snapshot.get("git_repositories") {
        Some(value) => match serde_json::from_value::<GitRepositoriesSnapshot>(value.clone()) {
            Ok(section) if section.schema_version == GIT_REPOSITORIES_SCHEMA_VERSION => section,
            _ => return,
        },
        None => GitRepositoriesSnapshot {
            schema_version: GIT_REPOSITORIES_SCHEMA_VERSION,
            repositories: Vec::new(),
        },
    };

    let mut seen_repositories = section
        .repositories
        .iter()
        .map(repository_key)
        .collect::<HashSet<_>>();
    let mut seen_candidates = HashSet::new();
    let mut candidate_count = 0usize;

    for vscode in recent_vscode_snapshots(canonical, now_ms) {
        let mut candidates = Vec::new();
        collect_vscode_candidates(&vscode, &mut candidates);
        for candidate in candidates {
            if candidate_count >= MAX_VSCODE_GIT_CANDIDATES {
                break;
            }
            if !seen_candidates.insert(location_key(&candidate)) {
                continue;
            }
            candidate_count += 1;

            let Some(repository) = probe_repository(&candidate) else {
                continue;
            };
            if seen_repositories.insert(repository_key(&repository)) {
                section.repositories.push(repository);
            }
        }
        if candidate_count >= MAX_VSCODE_GIT_CANDIDATES {
            break;
        }
    }

    if section.repositories.is_empty() {
        return;
    }
    if let Ok(value) = serde_json::to_value(section)
        && let Some(object) = snapshot.as_object_mut()
    {
        object.insert("git_repositories".to_owned(), value);
    }
}

fn recent_vscode_snapshots(canonical: &Path, now_ms: i64) -> Vec<Value> {
    let mut paths = vec![canonical.to_path_buf()];
    if let Some(parent) = canonical.parent()
        && let Ok(entries) = fs::read_dir(parent)
    {
        for entry in entries.flatten().take(MAX_VSCODE_RUNTIME_FILES) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("vscode-host-") && name.ends_with(".json") {
                paths.push(entry.path());
            }
        }
    }

    let mut snapshots = Vec::new();
    let mut seen_hosts = HashSet::new();
    for path in paths.into_iter().take(MAX_VSCODE_RUNTIME_FILES + 1) {
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        let Ok(envelope) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let Some(updated_at) = envelope.get("updatedAtUnixMs").and_then(Value::as_i64) else {
            continue;
        };
        if now_ms.saturating_sub(updated_at) > VSCODE_STATE_MAX_AGE.as_millis() as i64 {
            continue;
        }
        let Some(vscode) = envelope.get("snapshot") else {
            continue;
        };
        if vscode.get("schemaVersion").and_then(Value::as_u64)
            != Some(crate::vscode::VSCODE_SNAPSHOT_SCHEMA_VERSION as u64)
        {
            continue;
        }
        if !vscode_host_is_alive(vscode) {
            continue;
        }

        // Canonical vscode.json can mirror one per-host file. Avoid probing its
        // workspace twice while retaining independent host snapshots.
        let host_key = vscode
            .get("hostPid")
            .and_then(Value::as_u64)
            .map(|pid| format!("pid:{pid}"))
            .unwrap_or_else(|| {
                let workspace = vscode
                    .get("workspaceFolders")
                    .and_then(Value::as_array)
                    .map(|folders| {
                        folders
                            .iter()
                            .filter_map(|folder| folder.get("uri").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\u{1f}")
                    })
                    .unwrap_or_default();
                format!("workspace:{workspace}")
            });
        if seen_hosts.insert(host_key) {
            snapshots.push(vscode.clone());
        }
    }
    snapshots
}

fn collect_vscode_candidates(vscode: &Value, candidates: &mut Vec<VsCodeGitLocation>) {
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
                Some("wsl") => {
                    // Do not guess a non-default distro. A WSL workspace folder
                    // normally carries the distro in its vscode-remote URI.
                    if let Some(distro) = inferred_wsl_distro.clone() {
                        candidates.push(VsCodeGitLocation {
                            path: cwd.to_owned(),
                            environment: GitRepositoryEnvironment::Wsl {
                                distro: Some(distro),
                            },
                        });
                    }
                }
                Some(_) => {
                    // SSH/dev-container paths are not local filesystem paths and
                    // are intentionally not queried by the local CLI.
                }
                None => candidates.push(location_from_plain_path(cwd)),
            }
        }
    }
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

fn location_from_uri(uri: &str) -> Option<VsCodeGitLocation> {
    if let Some(path) = file_uri_to_path(uri) {
        return Some(location_from_plain_path(&path));
    }
    parse_wsl_remote_uri(uri).map(|(distro, path)| VsCodeGitLocation {
        path,
        environment: GitRepositoryEnvironment::Wsl {
            distro: Some(distro),
        },
    })
}

fn location_from_plain_path(path: &str) -> VsCodeGitLocation {
    if let Some((distro, wsl_path)) = parse_wsl_unc(path) {
        VsCodeGitLocation {
            path: wsl_path,
            environment: GitRepositoryEnvironment::Wsl {
                distro: Some(distro),
            },
        }
    } else {
        VsCodeGitLocation {
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
    Some((
        distro.to_owned(),
        if path.is_empty() {
            "/".to_owned()
        } else {
            format!("/{path}")
        },
    ))
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

fn probe_repository(location: &VsCodeGitLocation) -> Option<SavedGitRepository> {
    let root = git_text(location, &["rev-parse", "--show-toplevel"]).ok()?;
    let branch = git_text(location, &["branch", "--show-current"]).ok()?;
    let branch = branch.trim();
    if branch.is_empty() {
        return None;
    }
    Some(SavedGitRepository {
        repository_root: root.trim().to_owned(),
        branch: branch.to_owned(),
        environment: location.environment.clone(),
    })
}

fn git_text(location: &VsCodeGitLocation, args: &[&str]) -> Result<String, String> {
    let output = git_output(location, args)?;
    if !output.status.success() {
        return Err(command_error(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_output(location: &VsCodeGitLocation, args: &[&str]) -> Result<Output, String> {
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

fn git_command(location: &VsCodeGitLocation) -> io::Result<Command> {
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
                    "WSL Git context can only be captured on Windows",
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

fn location_key(location: &VsCodeGitLocation) -> String {
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
    location_key(&VsCodeGitLocation {
        path: repository.repository_root.clone(),
        environment: repository.environment.clone(),
    })
}

fn vscode_host_is_alive(snapshot: &Value) -> bool {
    let Some(pid) = snapshot.get("hostPid").and_then(Value::as_u64) else {
        return true;
    };
    let Ok(pid) = u32::try_from(pid) else {
        return false;
    };
    process_is_alive(pid)
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    unsafe {
        CloseHandle(handle);
    }
    true
}

#[cfg(not(windows))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
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
        std::env::temp_dir().join(format!(
            "context-capsule-vscode-git-{label}-{}-{}",
            std::process::id(),
            now_unix_ms()
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

    fn make_repo(root: &Path, name: &str, branch: &str) -> PathBuf {
        let repo = root.join(name);
        fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init"]);
        run(&repo, &["config", "user.email", "context-capsule@example.invalid"]);
        run(&repo, &["config", "user.name", "Context Capsule Tests"]);
        run(&repo, &["checkout", "-b", branch]);
        repo
    }

    fn file_uri(path: &Path) -> String {
        let value = path.to_string_lossy().replace('\\', "/");
        #[cfg(windows)]
        {
            format!("file:///{value}")
        }
        #[cfg(not(windows))]
        {
            format!("file://{value}")
        }
    }

    fn host_envelope(updated_at: i64, workspace_uri: &str) -> Value {
        json!({
            "updatedAtUnixMs": updated_at,
            "snapshot": {
                "schemaVersion": crate::vscode::VSCODE_SNAPSHOT_SCHEMA_VERSION,
                "hostPid": null,
                "remoteName": null,
                "workspaceFolders": [
                    { "uri": workspace_uri, "name": "project", "index": 0 }
                ],
                "integratedTerminals": []
            }
        })
    }

    #[test]
    fn independent_recent_vscode_hosts_contribute_their_git_branches() {
        if !git_available() {
            return;
        }
        let root = unique_temp_dir("multi-host");
        fs::create_dir_all(&root).unwrap();
        let first = make_repo(&root, "first", "feature-one");
        let second = make_repo(&root, "second", "feature-two");
        let stale = make_repo(&root, "stale", "old-branch");
        let runtime = root.join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        let canonical = runtime.join("vscode.json");
        let now = now_unix_ms();

        fs::write(
            runtime.join("vscode-host-1001.json"),
            serde_json::to_vec(&host_envelope(now, &file_uri(&first))).unwrap(),
        )
        .unwrap();
        fs::write(
            runtime.join("vscode-host-1002.json"),
            serde_json::to_vec(&host_envelope(now, &file_uri(&second))).unwrap(),
        )
        .unwrap();
        fs::write(
            runtime.join("vscode-host-1003.json"),
            serde_json::to_vec(&host_envelope(
                now - VSCODE_STATE_MAX_AGE.as_millis() as i64 - 1,
                &file_uri(&stale),
            ))
            .unwrap(),
        )
        .unwrap();

        let mut snapshot = json!({});
        augment_from_vscode_runtime_at(&mut snapshot, &canonical, now);
        let section: GitRepositoriesSnapshot = serde_json::from_value(
            snapshot.get("git_repositories").cloned().expect("git section"),
        )
        .unwrap();
        let branches = section
            .repositories
            .iter()
            .map(|repository| repository.branch.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(section.repositories.len(), 2);
        assert!(branches.contains("feature-one"));
        assert!(branches.contains("feature-two"));
        assert!(!branches.contains("old-branch"));

        let _ = fs::remove_dir_all(root);
    }
}
