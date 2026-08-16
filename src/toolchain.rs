use std::{fs, path::Path, process::Command, thread};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolVersion {
    pub name: String,
    pub command: String,
    pub version: String,
    pub executable_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionHint {
    pub source: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy)]
struct ToolSpec {
    name: &'static str,
    command: &'static str,
    args: &'static [&'static str],
}

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "Git",
        command: "git",
        args: &["--version"],
    },
    ToolSpec {
        name: "Node.js",
        command: "node",
        args: &["--version"],
    },
    ToolSpec {
        name: "npm",
        command: "npm",
        args: &["--version"],
    },
    ToolSpec {
        name: "pnpm",
        command: "pnpm",
        args: &["--version"],
    },
    ToolSpec {
        name: "npx",
        command: "npx",
        args: &["--version"],
    },
    ToolSpec {
        name: "Corepack",
        command: "corepack",
        args: &["--version"],
    },
    ToolSpec {
        name: "Yarn",
        command: "yarn",
        args: &["--version"],
    },
    ToolSpec {
        name: "Bun",
        command: "bun",
        args: &["--version"],
    },
    ToolSpec {
        name: "Deno",
        command: "deno",
        args: &["--version"],
    },
    ToolSpec {
        name: "Rust",
        command: "rustc",
        args: &["--version"],
    },
    ToolSpec {
        name: "Cargo",
        command: "cargo",
        args: &["--version"],
    },
    ToolSpec {
        name: "rustup",
        command: "rustup",
        args: &["--version"],
    },
    ToolSpec {
        name: "Python",
        command: "python",
        args: &["--version"],
    },
    ToolSpec {
        name: "Python 3",
        command: "python3",
        args: &["--version"],
    },
    ToolSpec {
        name: "pip",
        command: "pip",
        args: &["--version"],
    },
    ToolSpec {
        name: "uv",
        command: "uv",
        args: &["--version"],
    },
    ToolSpec {
        name: "Poetry",
        command: "poetry",
        args: &["--version"],
    },
    ToolSpec {
        name: "Go",
        command: "go",
        args: &["version"],
    },
    ToolSpec {
        name: ".NET SDK",
        command: "dotnet",
        args: &["--version"],
    },
    ToolSpec {
        name: "Java",
        command: "java",
        args: &["-version"],
    },
    ToolSpec {
        name: "Maven",
        command: "mvn",
        args: &["--version"],
    },
    ToolSpec {
        name: "Gradle",
        command: "gradle",
        args: &["--version"],
    },
    ToolSpec {
        name: "PHP",
        command: "php",
        args: &["--version"],
    },
    ToolSpec {
        name: "Composer",
        command: "composer",
        args: &["--version"],
    },
    ToolSpec {
        name: "Ruby",
        command: "ruby",
        args: &["--version"],
    },
    ToolSpec {
        name: "Bundler",
        command: "bundle",
        args: &["--version"],
    },
    ToolSpec {
        name: "Docker",
        command: "docker",
        args: &["--version"],
    },
    ToolSpec {
        name: "Docker Compose",
        command: "docker",
        args: &["compose", "version", "--short"],
    },
    ToolSpec {
        name: "Podman",
        command: "podman",
        args: &["--version"],
    },
    ToolSpec {
        name: "CMake",
        command: "cmake",
        args: &["--version"],
    },
    ToolSpec {
        name: "Ninja",
        command: "ninja",
        args: &["--version"],
    },
    ToolSpec {
        name: "GCC",
        command: "gcc",
        args: &["--version"],
    },
    ToolSpec {
        name: "Clang",
        command: "clang",
        args: &["--version"],
    },
    ToolSpec {
        name: "PowerShell",
        command: "pwsh",
        args: &["--version"],
    },
    ToolSpec {
        name: "WSL",
        command: "wsl",
        args: &["--version"],
    },
    ToolSpec {
        name: "kubectl",
        command: "kubectl",
        args: &["version", "--client"],
    },
    ToolSpec {
        name: "Terraform",
        command: "terraform",
        args: &["version"],
    },
    ToolSpec {
        name: "GitHub CLI",
        command: "gh",
        args: &["--version"],
    },
    ToolSpec {
        name: "fnm",
        command: "fnm",
        args: &["--version"],
    },
    ToolSpec {
        name: "Volta",
        command: "volta",
        args: &["--version"],
    },
];

const VERSION_HINT_FILES: &[&str] = &[
    ".nvmrc",
    ".node-version",
    ".python-version",
    ".ruby-version",
    ".java-version",
    ".tool-versions",
    ".sdkmanrc",
    ".mise.toml",
    "mise.toml",
    "rust-toolchain",
    "rust-toolchain.toml",
    "global.json",
    "gradle/wrapper/gradle-wrapper.properties",
];

const MAX_VERSION_HINT_BYTES: u64 = 32 * 1024;

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

pub fn discover_version_hints(project_root: &Path) -> Vec<VersionHint> {
    VERSION_HINT_FILES
        .iter()
        .filter_map(|relative_path| {
            let path = project_root.join(relative_path);
            let metadata = fs::metadata(&path).ok()?;
            if !metadata.is_file() || metadata.len() > MAX_VERSION_HINT_BYTES {
                return None;
            }

            let value = fs::read_to_string(&path).ok()?.trim().to_owned();
            if value.is_empty() {
                return None;
            }

            Some(VersionHint {
                source: (*relative_path).to_owned(),
                value,
            })
        })
        .collect()
}

fn discover_one(spec: ToolSpec) -> Option<ToolVersion> {
    let executable_path = resolve_executable(spec.command)?;
    let output = run_version_command(spec.command, spec.args)?;
    let version =
        first_non_empty_line(&output.stdout).or_else(|| first_non_empty_line(&output.stderr))?;

    Some(ToolVersion {
        name: spec.name.to_owned(),
        command: spec.command.to_owned(),
        version,
        executable_path: Some(executable_path),
    })
}

#[cfg(windows)]
fn run_version_command(command: &str, args: &[&str]) -> Option<std::process::Output> {
    if let Ok(output) = Command::new(command).args(args).output() {
        return Some(output);
    }

    // npm, pnpm, npx, Corepack and similar Windows tools are frequently .cmd shims.
    let command_line = std::iter::once(command)
        .chain(args.iter().copied())
        .map(quote_cmd_argument)
        .collect::<Vec<_>>()
        .join(" ");

    Command::new("cmd.exe")
        .args(["/D", "/S", "/C"])
        .arg(command_line)
        .output()
        .ok()
}

#[cfg(not(windows))]
fn run_version_command(command: &str, args: &[&str]) -> Option<std::process::Output> {
    Command::new(command).args(args).output().ok()
}

#[cfg(windows)]
fn quote_cmd_argument(argument: &str) -> String {
    if argument
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_.:/".contains(character))
    {
        argument.to_owned()
    } else {
        format!("\"{}\"", argument.replace('"', "\"\""))
    }
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
    select_windows_launch_path(&output.stdout)
}

#[cfg(windows)]
fn select_windows_launch_path(bytes: &[u8]) -> Option<String> {
    let candidates = String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    for extension in [".exe", ".com", ".cmd", ".bat"] {
        if let Some(candidate) = candidates
            .iter()
            .find(|path| path.to_ascii_lowercase().ends_with(extension))
        {
            return Some(candidate.clone());
        }
    }

    candidates.into_iter().next()
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
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

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

    #[test]
    fn reads_only_whitelisted_project_version_hints() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "context-capsule-version-hints-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create test directory");
        fs::write(root.join(".nvmrc"), "22.14.0\n").expect("write nvmrc");
        fs::write(root.join(".npmrc"), "//registry/:_authToken=secret\n")
            .expect("write excluded npmrc");

        let hints = discover_version_hints(&root);
        fs::remove_dir_all(&root).expect("remove test directory");

        assert_eq!(
            hints,
            vec![VersionHint {
                source: ".nvmrc".to_owned(),
                value: "22.14.0".to_owned(),
            }]
        );
    }

    #[cfg(windows)]
    #[test]
    fn cmd_arguments_only_quote_when_needed() {
        assert_eq!(quote_cmd_argument("--version"), "--version");
        assert_eq!(quote_cmd_argument("hello world"), "\"hello world\"");
    }

    #[cfg(windows)]
    #[test]
    fn windows_launcher_prefers_executable_or_cmd_shim() {
        let output = b"C:\\Program Files\\nodejs\\npm\r\nC:\\Program Files\\nodejs\\npm.cmd\r\n";
        assert_eq!(
            select_windows_launch_path(output),
            Some(r"C:\Program Files\nodejs\npm.cmd".to_owned())
        );
    }
}
