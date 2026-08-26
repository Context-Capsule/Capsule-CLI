#[path = "git_context_v2.rs"]
mod previous;

pub use previous::{
    GitRepositoriesSnapshot, GitRepositoryEnvironment, GitRestoreReport, SavedGitRepository,
};

use serde_json::{Value, json};

const GIT_REPOSITORIES_SCHEMA_VERSION: u32 = 1;
const MAX_REPOSITORIES: usize = 128;

/// Captures branch-only Git context from VS Code, the serialized terminal
/// snapshot, a fresh read-only live-terminal pass, and the CLI invocation
/// directory.
///
/// The live-terminal pass matters because the durable terminal snapshot is
/// intentionally filtered for restore safety (for example the shell hosting
/// `capsule save` is not persisted as a terminal to reopen). Git context is
/// independent of that restart filtering, so every safely observable live
/// terminal CWD should still be allowed to contribute its repository branch.
pub fn capture_into_snapshot(snapshot: &mut Value) {
    previous::capture_into_snapshot(snapshot);
    augment_from_live_terminals(snapshot);
    augment_from_invoking_terminal(snapshot);
}

pub fn restore_from_snapshot(snapshot: &Value, dry_run: bool) -> GitRestoreReport {
    previous::restore_from_snapshot(snapshot, dry_run)
}

fn augment_from_live_terminals(snapshot: &mut Value) {
    // Re-discover at the persistence boundary so Git capture is not limited by
    // the terminal sessions that were intentionally kept/removed for restore.
    // enrich_for_matching is read-only and gives Windows Terminal PowerShell
    // the same exact-CWD treatment already used by the terminal restore engine.
    let discovered = crate::adapters::terminal::discover();
    let enriched = crate::terminal_context::enrich_for_matching(&discovered);
    let Ok(terminals) = serde_json::to_value(enriched) else {
        return;
    };
    augment_from_terminal_value(snapshot, terminals);
}

fn augment_from_terminal_value(snapshot: &mut Value, terminals: Value) {
    // Feed the live terminal inventory through the already-tested branch
    // probing implementation. This executes Git as a separate process using
    // `git -C <terminal-cwd> ...`; it never types Git commands into a user's
    // terminal and never mutates the terminal session during capture.
    let mut terminal_only_snapshot = json!({
        "terminals": terminals,
        "editors": { "vscode": null }
    });
    previous::capture_into_snapshot(&mut terminal_only_snapshot);
    merge_git_section(snapshot, &terminal_only_snapshot);
}

fn augment_from_invoking_terminal(snapshot: &mut Value) {
    let Some(git) = snapshot.get("git") else {
        return;
    };
    if git.get("status").and_then(Value::as_str) != Some("repository") {
        return;
    }

    let Some(repository_root) = git
        .get("repository_root")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    let Some(branch) = git
        .get("branch")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        // Detached HEAD remains intentionally unsupported: only named branches
        // are captured/restored by this feature.
        return;
    };

    merge_repository(
        snapshot,
        SavedGitRepository {
            repository_root: repository_root.to_owned(),
            branch: branch.to_owned(),
            environment: GitRepositoryEnvironment::Local,
        },
    );
}

fn merge_git_section(target: &mut Value, source: &Value) {
    let Some(value) = source.get("git_repositories") else {
        return;
    };
    let Ok(section) = serde_json::from_value::<GitRepositoriesSnapshot>(value.clone()) else {
        return;
    };
    if section.schema_version != GIT_REPOSITORIES_SCHEMA_VERSION {
        return;
    }

    for repository in section.repositories {
        merge_repository(target, repository);
    }
}

fn merge_repository(snapshot: &mut Value, repository: SavedGitRepository) {
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

    if section
        .repositories
        .iter()
        .any(|existing| same_repository(existing, &repository))
    {
        return;
    }
    if section.repositories.len() >= MAX_REPOSITORIES {
        return;
    }

    section.repositories.push(repository);
    let Ok(value) = serde_json::to_value(section) else {
        return;
    };
    if let Some(object) = snapshot.as_object_mut() {
        object.insert("git_repositories".to_owned(), value);
    }
}

fn same_repository(left: &SavedGitRepository, right: &SavedGitRepository) -> bool {
    if left.environment != right.environment {
        return false;
    }

    let left = normalized_repository_path(&left.repository_root, &left.environment);
    let right = normalized_repository_path(&right.repository_root, &right.environment);
    left == right
}

fn normalized_repository_path(path: &str, environment: &GitRepositoryEnvironment) -> String {
    let mut path = path.trim_end_matches(['/', '\\']).replace('\\', "/");
    #[cfg(windows)]
    if matches!(environment, GitRepositoryEnvironment::Local) {
        path.make_ascii_lowercase();
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, process::Command, time::{SystemTime, UNIX_EPOCH}};

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
            "context-capsule-git-v3-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn run(repo: &PathBuf, args: &[&str]) {
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

    fn make_repo(label: &str, branch: &str) -> Option<PathBuf> {
        if !git_available() {
            return None;
        }
        let repo = unique_temp_dir(label);
        fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init"]);
        run(&repo, &["config", "user.email", "context-capsule@example.invalid"]);
        run(&repo, &["config", "user.name", "Context Capsule Tests"]);
        run(&repo, &["checkout", "-b", branch]);
        fs::write(repo.join("tracked.txt"), "initial\n").unwrap();
        run(&repo, &["add", "tracked.txt"]);
        run(&repo, &["commit", "-m", "initial"]);
        Some(repo)
    }

    #[test]
    fn invoking_terminal_git_repo_is_added_even_without_saved_terminal_session() {
        let mut snapshot = json!({
            "git": {
                "status": "repository",
                "repository_root": "C:/work/terminal-project",
                "branch": "terminal-saved-branch",
                "head": "ignored-by-branch-context"
            },
            "terminals": { "sessions": [] },
            "editors": { "vscode": null }
        });

        augment_from_invoking_terminal(&mut snapshot);

        let repositories = snapshot["git_repositories"]["repositories"]
            .as_array()
            .expect("repositories");
        assert_eq!(repositories.len(), 1);
        assert_eq!(repositories[0]["repository_root"], "C:/work/terminal-project");
        assert_eq!(repositories[0]["branch"], "terminal-saved-branch");
        assert!(repositories[0].get("head").is_none());
        assert!(repositories[0].get("commit").is_none());
    }

    #[test]
    fn live_terminal_value_contributes_repo_omitted_from_durable_terminal_snapshot() {
        let Some(repo) = make_repo("live-terminal", "terminal-dev") else {
            return;
        };
        let mut snapshot = json!({
            "git": { "status": "not-repository" },
            "terminals": { "sessions": [] },
            "editors": { "vscode": null }
        });
        let terminals = json!({
            "status": "available",
            "message": null,
            "windows_terminal_layouts": [],
            "sessions": [{
                "sources": ["windows-process"],
                "host": "windows-terminal",
                "shell": "power-shell",
                "shell_executable": "pwsh.exe",
                "environment": { "kind": "windows" },
                "pid": 4242,
                "parent_pid": null,
                "tty": null,
                "profile": "PowerShell",
                "title": null,
                "working_directory": repo.to_string_lossy(),
                "working_directory_source": "windows-terminal-state",
                "startup_command": null,
                "foreground_command": null,
                "restart": null
            }],
            "warnings": [],
            "history": { "captured": false, "reason": "test" }
        });

        augment_from_terminal_value(&mut snapshot, terminals);

        let repositories = snapshot["git_repositories"]["repositories"]
            .as_array()
            .expect("repositories");
        assert!(repositories.iter().any(|repository| {
            repository["branch"] == "terminal-dev"
                && repository["repository_root"]
                    .as_str()
                    .is_some_and(|value| value == repo.to_string_lossy())
        }));
        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn invoking_terminal_repo_does_not_duplicate_existing_terminal_or_vscode_repo() {
        let mut snapshot = json!({
            "git": {
                "status": "repository",
                "repository_root": "C:/work/project",
                "branch": "dev"
            },
            "git_repositories": {
                "schema_version": 1,
                "repositories": [{
                    "repository_root": "C:/work/project",
                    "branch": "dev",
                    "environment": { "kind": "local" }
                }]
            }
        });

        augment_from_invoking_terminal(&mut snapshot);

        assert_eq!(
            snapshot["git_repositories"]["repositories"]
                .as_array()
                .expect("repositories")
                .len(),
            1
        );
    }

    #[test]
    fn detached_invoking_terminal_is_not_added() {
        let mut snapshot = json!({
            "git": {
                "status": "repository",
                "repository_root": "C:/work/project",
                "branch": null
            }
        });

        augment_from_invoking_terminal(&mut snapshot);
        assert!(snapshot.get("git_repositories").is_none());
    }
}
