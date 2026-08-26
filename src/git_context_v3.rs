#[path = "git_context_v2.rs"]
mod previous;

pub use previous::{
    GitRepositoriesSnapshot, GitRepositoryEnvironment, GitRestoreReport, SavedGitRepository,
};

use serde_json::Value;

const GIT_REPOSITORIES_SCHEMA_VERSION: u32 = 1;

/// Captures branch-only Git context from VS Code, saved terminal sessions, and
/// the CLI invocation directory. The invocation shell is intentionally omitted
/// from the durable terminal session list so it is not reopened as a duplicate,
/// but its existing top-level `git` snapshot still represents a real terminal
/// CWD that must participate in branch restoration.
pub fn capture_into_snapshot(snapshot: &mut Value) {
    previous::capture_into_snapshot(snapshot);
    augment_from_invoking_terminal(snapshot);
}

pub fn restore_from_snapshot(snapshot: &Value, dry_run: bool) -> GitRestoreReport {
    previous::restore_from_snapshot(snapshot, dry_run)
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

    let repository = SavedGitRepository {
        repository_root: repository_root.to_owned(),
        branch: branch.to_owned(),
        environment: GitRepositoryEnvironment::Local,
    };

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
    use serde_json::json;

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
