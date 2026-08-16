use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitContext {
    pub repository_root: String,
    pub remote_origin: Option<String>,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub dirty: bool,
    pub changed_files: Vec<String>,
    pub stash_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitDiscoveryError {
    NotInstalled,
    NotRepository,
}

pub fn discover_current() -> Result<GitContext, GitDiscoveryError> {
    let repository_root = git_required(&["rev-parse", "--show-toplevel"])?;
    let branch = git_optional(&["branch", "--show-current"]).filter(|value| !value.is_empty());
    let head = git_optional(&["rev-parse", "HEAD"]);
    let remote_origin = git_optional(&["remote", "get-url", "origin"]);
    let status = git_optional(&["status", "--porcelain=v1"]).unwrap_or_default();
    let changed_files = parse_changed_files(&status);
    let stash_count = git_optional(&["stash", "list"])
        .map(|output| output.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0);

    Ok(GitContext {
        repository_root,
        remote_origin,
        branch,
        head,
        dirty: !changed_files.is_empty(),
        changed_files,
        stash_count,
    })
}

fn git_required(args: &[&str]) -> Result<String, GitDiscoveryError> {
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|_| GitDiscoveryError::NotInstalled)?;

    if !output.status.success() {
        return Err(GitDiscoveryError::NotRepository);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_optional(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn parse_changed_files(status: &str) -> Vec<String> {
    status
        .lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }

            let path = line[3..].trim();
            if path.is_empty() {
                None
            } else {
                Some(path.to_owned())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modified_untracked_and_renamed_files() {
        let changed = parse_changed_files(
            " M src/main.rs\n?? notes.txt\nR  old-name.rs -> new-name.rs\n",
        );

        assert_eq!(
            changed,
            vec![
                "src/main.rs".to_owned(),
                "notes.txt".to_owned(),
                "old-name.rs -> new-name.rs".to_owned(),
            ]
        );
    }

    #[test]
    fn empty_status_is_clean() {
        assert!(parse_changed_files("").is_empty());
    }
}
