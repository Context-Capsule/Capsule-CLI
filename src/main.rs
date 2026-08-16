use std::{
    env,
    process::{Command, ExitCode},
};

fn main() -> ExitCode {
    let mut args = env::args().skip(1);

    match args.next().as_deref() {
        Some("inspect") => {
            if args.next().is_some() {
                eprintln!("error: 'inspect' does not accept arguments\n");
                print_usage();
                return ExitCode::from(2);
            }

            inspect()
        }
        Some("-h") | Some("--help") | None => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some(command) => {
            eprintln!("error: unknown command '{command}'\n");
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn inspect() -> ExitCode {
    let current_dir = match env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("Failed to determine current directory: {error}");
            return ExitCode::from(1);
        }
    };

    println!("Current directory: {}", current_dir.display());

    match git_output(&["rev-parse", "--show-toplevel"]) {
        Ok(repo_root) => {
            println!("Git repository:    {repo_root}");

            match git_output(&["branch", "--show-current"]) {
                Ok(branch) if !branch.is_empty() => println!("Git branch:        {branch}"),
                Ok(_) => println!("Git branch:        (detached HEAD)"),
                Err(_) => println!("Git branch:        unavailable"),
            }

            match git_output(&["rev-parse", "HEAD"]) {
                Ok(head) => println!("Git HEAD:          {head}"),
                Err(_) => println!("Git HEAD:          unavailable"),
            }
        }
        Err(GitCommandError::NotInstalled) => {
            println!("Git repository:    unavailable (git not installed)");
        }
        Err(GitCommandError::Failed) => {
            println!("Git repository:    not detected");
        }
    }

    ExitCode::SUCCESS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitCommandError {
    NotInstalled,
    Failed,
}

fn git_output(args: &[&str]) -> Result<String, GitCommandError> {
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|_| GitCommandError::NotInstalled)?;

    if !output.status.success() {
        return Err(GitCommandError::Failed);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn print_usage() {
    println!("Context Capsule CLI\n");
    println!("Usage:");
    println!("  context-capsule inspect\n");
    println!("Commands:");
    println!("  inspect    Show the current directory and Git context");
}
