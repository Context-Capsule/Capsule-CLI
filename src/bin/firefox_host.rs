use context_capsule::browser;
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();

    if arguments.is_empty() || is_native_messaging_invocation(&arguments) {
        return run_protocol();
    }

    match arguments.as_slice() {
        [option] if option == "--install" => match browser::install_native_host() {
            Ok(path) => match doctor() {
                Ok(report) => {
                    println!("Installed and verified Firefox/Zen native messaging host.");
                    print_doctor_report(&report);
                    println!("  extension: {}", browser::FIREFOX_EXTENSION_ID);
                    println!("  manifest: {}", path.display());
                    ExitCode::SUCCESS
                }
                Err(error) => fail(format!(
                    "native host files were written, but validation failed: {error}"
                )),
            },
            Err(error) => fail(error.to_string()),
        },
        [option] if option == "--doctor" => match doctor() {
            Ok(report) => {
                println!("Firefox/Zen native messaging host: healthy");
                print_doctor_report(&report);
                ExitCode::SUCCESS
            }
            Err(error) => fail(format!("native host is not healthy: {error}")),
        },
        [option] if option == "--uninstall" => match browser::uninstall_native_host() {
            Ok(path) => {
                println!("Removed Firefox/Zen native messaging host registration.");
                println!("  manifest: {}", path.display());
                ExitCode::SUCCESS
            }
            Err(error) => fail(error.to_string()),
        },
        [option] if option == "--status" => match browser::load_recent_firefox_state() {
            Ok(Some(snapshot)) => {
                println!("Firefox/Zen adapter: live");
                println!("  windows: {}", snapshot.windows.len());
                println!("  tabs: {}", snapshot.tab_count());
                println!("  extension: {}", snapshot.extension_version);
                ExitCode::SUCCESS
            }
            Ok(None) => {
                println!("Firefox/Zen adapter: no recent state");
                ExitCode::SUCCESS
            }
            Err(error) => fail(error.to_string()),
        },
        [option] if option == "-h" || option == "--help" => {
            print_usage();
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("error: invalid native-host arguments: {arguments:?}\n");
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn run_protocol() -> ExitCode {
    match browser::run_native_host() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Context Capsule Firefox/Zen native host failed: {error}");
            ExitCode::from(1)
        }
    }
}

fn is_native_messaging_invocation(arguments: &[String]) -> bool {
    if arguments.len() != 2 || arguments[1] != browser::FIREFOX_EXTENSION_ID {
        return false;
    }

    Path::new(&arguments[0])
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.eq_ignore_ascii_case(&format!("{}.json", browser::NATIVE_HOST_NAME))
        })
}

#[derive(Debug)]
struct DoctorReport {
    manifest_path: PathBuf,
    executable_path: PathBuf,
    #[cfg(windows)]
    registry_manifest_path: PathBuf,
}

fn doctor() -> Result<DoctorReport, String> {
    let manifest_path = browser::native_manifest_path().map_err(|error| error.to_string())?;
    if !manifest_path.is_file() {
        return Err(format!(
            "manifest is missing at '{}'. Run --install first",
            manifest_path.display()
        ));
    }

    let manifest_bytes = fs::read(&manifest_path)
        .map_err(|error| format!("cannot read '{}': {error}", manifest_path.display()))?;
    let manifest: Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| format!("manifest is invalid JSON: {error}"))?;

    require_string(&manifest, "name", browser::NATIVE_HOST_NAME)?;
    require_string(&manifest, "type", "stdio")?;

    let allowed = manifest
        .get("allowed_extensions")
        .and_then(Value::as_array)
        .ok_or_else(|| "manifest has no allowed_extensions array".to_owned())?;
    if allowed.len() != 1 || allowed[0].as_str() != Some(browser::FIREFOX_EXTENSION_ID) {
        return Err(format!(
            "manifest must authorize only '{}'",
            browser::FIREFOX_EXTENSION_ID
        ));
    }

    let executable_path = manifest
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "manifest has no executable path".to_owned())?;
    if !executable_path.is_file() {
        return Err(format!(
            "manifest points to missing executable '{}'",
            executable_path.display()
        ));
    }

    #[cfg(windows)]
    let registry_manifest_path = {
        let registered = read_windows_registration()?;
        if !same_windows_path(&registered, &manifest_path) {
            return Err(format!(
                "Windows registry points to '{}' instead of '{}'",
                registered.display(),
                manifest_path.display()
            ));
        }
        registered
    };

    probe_native_host(&executable_path, &manifest_path)?;

    Ok(DoctorReport {
        manifest_path,
        executable_path,
        #[cfg(windows)]
        registry_manifest_path,
    })
}

fn require_string(manifest: &Value, field: &str, expected: &str) -> Result<(), String> {
    match manifest.get(field).and_then(Value::as_str) {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(format!(
            "manifest field '{field}' is '{actual}', expected '{expected}'"
        )),
        None => Err(format!("manifest field '{field}' is missing")),
    }
}

fn probe_native_host(executable: &Path, manifest_path: &Path) -> Result<(), String> {
    let mut child = Command::new(executable)
        .arg(manifest_path)
        .arg(browser::FIREFOX_EXTENSION_ID)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!(
                "cannot start native host '{}': {error}",
                executable.display()
            )
        })?;

    let request = serde_json::to_vec(&json!({
        "protocol_version": browser::NATIVE_PROTOCOL_VERSION,
        "request_id": "doctor",
        "type": "ping"
    }))
    .map_err(|error| format!("cannot encode doctor ping: {error}"))?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "native host stdin was not available".to_owned())?;
        let length = u32::try_from(request.len())
            .map_err(|_| "doctor ping is unexpectedly large".to_owned())?;
        stdin
            .write_all(&length.to_le_bytes())
            .and_then(|_| stdin.write_all(&request))
            .map_err(|error| format!("cannot send doctor ping: {error}"))?;
    }
    drop(child.stdin.take());

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "native host stdout was not available".to_owned())?;
    let mut length_bytes = [0_u8; 4];
    stdout
        .read_exact(&mut length_bytes)
        .map_err(|error| format!("native host returned no framed response: {error}"))?;
    let length = u32::from_le_bytes(length_bytes) as usize;
    if length == 0 || length > 1024 * 1024 {
        return Err(format!(
            "native host returned invalid response length {length}"
        ));
    }
    let mut payload = vec![0_u8; length];
    stdout
        .read_exact(&mut payload)
        .map_err(|error| format!("native host response was incomplete: {error}"))?;
    let response: Value = serde_json::from_slice(&payload)
        .map_err(|error| format!("native host response was invalid JSON: {error}"))?;

    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for native host probe: {error}"))?;
    if !status.success() {
        return Err(format!("native host probe exited with {status}"));
    }
    if response.get("ok").and_then(Value::as_bool) != Some(true)
        || response.get("type").and_then(Value::as_str) != Some("pong")
        || response.get("request_id").and_then(Value::as_str) != Some("doctor")
    {
        return Err(format!("unexpected native host ping response: {response}"));
    }
    Ok(())
}

#[cfg(windows)]
fn read_windows_registration() -> Result<PathBuf, String> {
    let key = format!(
        r"HKCU\Software\Mozilla\NativeMessagingHosts\{}",
        browser::NATIVE_HOST_NAME
    );
    let output = Command::new("reg.exe")
        .args(["query", &key, "/ve"])
        .output()
        .map_err(|error| format!("cannot query native-host registry key: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Windows registry key '{}' is missing. Run --install first. {}",
            key,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_registry_default(&String::from_utf8_lossy(&output.stdout))
        .map(PathBuf::from)
        .ok_or_else(|| format!("Windows registry key '{key}' has no default REG_SZ value"))
}

#[cfg(windows)]
fn same_windows_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .trim_matches('"')
        .eq_ignore_ascii_case(right.to_string_lossy().trim_matches('"'))
}

#[cfg(windows)]
fn parse_registry_default(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (_, value) = line.split_once("REG_SZ")?;
        let value = value.trim().trim_matches('"');
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn print_doctor_report(report: &DoctorReport) {
    println!("  manifest: {}", report.manifest_path.display());
    println!("  executable: {}", report.executable_path.display());
    #[cfg(windows)]
    println!(
        "  registry: HKCU\\Software\\Mozilla\\NativeMessagingHosts\\{} -> {}",
        browser::NATIVE_HOST_NAME,
        report.registry_manifest_path.display()
    );
    println!("  browser-style protocol ping: ok");
}

fn fail(message: String) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(1)
}

fn print_usage() {
    println!("Context Capsule Firefox/Zen native host");
    println!();
    println!("Usage:");
    println!("  capsule-firefox-host --install");
    println!("  capsule-firefox-host --doctor");
    println!("  capsule-firefox-host --status");
    println!("  capsule-firefox-host --uninstall");
    println!();
    println!("--install writes the native manifest, registers it, and validates the result.");
    println!(
        "--doctor verifies the manifest, executable, Windows registration, and the exact browser-style protocol launch."
    );
    println!();
    println!(
        "Firefox/Zen also launches this executable internally with the native manifest path and extension ID; those arguments enter protocol mode automatically."
    );
}

#[cfg(test)]
mod tests {
    use super::is_native_messaging_invocation;
    use context_capsule::browser;

    #[test]
    fn recognizes_firefox_browser_invocation_arguments() {
        let arguments = vec![
            format!(r"C:\temp\{}.json", browser::NATIVE_HOST_NAME),
            browser::FIREFOX_EXTENSION_ID.to_owned(),
        ];
        assert!(is_native_messaging_invocation(&arguments));
    }

    #[test]
    fn rejects_wrong_extension_or_manifest_name() {
        let wrong_extension = vec![
            format!(r"C:\temp\{}.json", browser::NATIVE_HOST_NAME),
            "other@example.test".to_owned(),
        ];
        assert!(!is_native_messaging_invocation(&wrong_extension));

        let wrong_manifest = vec![
            r"C:\temp\other-host.json".to_owned(),
            browser::FIREFOX_EXTENSION_ID.to_owned(),
        ];
        assert!(!is_native_messaging_invocation(&wrong_manifest));
    }

    #[cfg(windows)]
    #[test]
    fn parses_windows_registry_default_value() {
        let output = r#"
HKEY_CURRENT_USER\Software\Mozilla\NativeMessagingHosts\com.contextcapsule.host
    (Default)    REG_SZ    C:\Users\test\AppData\Local\ContextCapsule\native-messaging\com.contextcapsule.host.json
"#;
        assert_eq!(
            super::parse_registry_default(output).as_deref(),
            Some(
                r"C:\Users\test\AppData\Local\ContextCapsule\native-messaging\com.contextcapsule.host.json"
            )
        );
    }
}
