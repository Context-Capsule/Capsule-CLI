use std::{process::Command, thread};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolVersion {
    pub name: String,
    pub command: String,
    pub version: String,
    pub executable_path: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct ToolSpec {
    name: &'static str,
    command: &'static str,
    args: &'static [&'static str],
}

const TOOLS: &[ToolSpec] = &[
    ToolSpec { name: "Git", command: "git", args: &["--version"] },
    ToolSpec { name: "Node.js", command: "node", args: &["--version"] },
    ToolSpec { name: "npm", command: "npm", args: &["--version"] },
    ToolSpec { name: "pnpm", command: "pnpm", args: &["--version"] },
    ToolSpec { name: "npx", command: "npx", args: &["--version"] },
    ToolSpec { name: "Corepack", command: "corepack", args: &["--version"] },
    ToolSpec { name: "Yarn", command: "yarn", args: &["--version"] },
    ToolSpec { name: "Bun", command: "bun", args: &["--version"] },
    ToolSpec { name: "Deno", command: "deno", args: &["--version"] },
    ToolSpec { name: "Rust", command: "rustc", args: &["--version"] },
    ToolSpec { name: "Cargo", command: "cargo", args: &["--version"] },
    ToolSpec { name: "rustup", command: "rustup", args: &["--version"] },
    ToolSpec { name: "Python", command: "python", args: &["--version"] },
    ToolSpec { name: "Python 3", command: "python3", args: &["--version"] },
    ToolSpec { name: "pip", command: "pip", args: &["--version"] },
    ToolSpec { name: "uv", command: "uv", args: &["--version"] },
    ToolSpec { name: "Poetry", command: "poetry", args: &["--version"] },
    ToolSpec { name: "Go", command: "go", args: &["version"] },
    ToolSpec { name: ".NET SDK", command: "dotnet", args: &["--version"] },
    ToolSpec { name: "Java", command: "java", args: &["-version"] },
    ToolSpec { name: "Docker", command: "docker", args: &["--version"] },
    ToolSpec { name: "Docker Compose", command: "docker", args: &["compose", "version", "--short"] },
    ToolSpec { name: "Podman", command: "podman", args: &["--version"] },
    ToolSpec { name: "CMake", command: "cmake", args: &["--version"] },
    ToolSpec { name: "Ninja", command: "ninja", args: &["--version"] },
    ToolSpec { name: "GCC", command: "gcc", args: &["--version"] },
    ToolSpec { name: "Clang", command: "clang", args: &["--version"] },
    ToolSpec { name: "PowerShell", command: "pwsh", args: &["--version"] },
    ToolSpec { name: "fnm", command: "fnm", args: &["--version"] },
    ToolSpec { name: "Volta", command: "volta", args: &["--version"] },
];

pub fn discover_tools() -> Vec<ToolVersion> {
    let mut discovered = thread::scope(|scope| {
        let handles: Vec<_> = TOOLS
            .iter()
            .copied()
            .map(|spec| scope.spawn(move || discover_one(spec)))
            .collect();

        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok().flatten())
            .collect::<Vec<_>>()
    });

    discovered.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
    });
    discovered
}

fn discover_one(spec: ToolSpec) -> Option<ToolVersion> {
    let output = Command::new(spec.command).args(spec.args).output().ok()?;
    let version = first_non_empty_line(&output.stdout)
        .or_else(|| first_non_empty_line(&output.stderr))?;

    Some(ToolVersion {
        name: spec.name.to_owned(),
        command: spec.command.to_owned(),
        version,
        executable_path: resolve_executable(spec.command),
    })
}

fn first_non_empty_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(windows)]
fn resolve_executable(command: &str) -> Option<String> {
    let output = Command::new("where.exe").arg(command).output().ok()?;
    if !output.status.success() {
        return None;
    }
    first_non_empty_line(&output.stdout)
}

#[cfg(not(windows))]
fn resolve_executable(command: &str) -> Option<String> {
    let output = Command::new("which").arg(command).output().ok()?;
    if !output.status.success() {
        return None;
    }
    first_non_empty_line(&output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_first_non_empty_stdout_line() {
        assert_eq!(
            first_non_empty_line(b"\n\nv24.1.0\nmore\n"),
            Some("v24.1.0".to_owned())
        );
    }

    #[test]
    fn empty_output_has_no_version() {
        assert_eq!(first_non_empty_line(b"\r\n \n"), None);
    }

    #[test]
    fn important_javascript_tools_are_part_of_discovery() {
        for command in ["node", "npm", "pnpm", "npx", "corepack", "yarn"] {
            assert!(
                TOOLS.iter().any(|tool| tool.command == command),
                "{command} should be discovered"
            );
        }
    }
}
