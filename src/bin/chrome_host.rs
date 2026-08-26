use context_capsule::chrome;
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
        [option] if option == "--install" => match chrome::install_native_host() {
            Ok(path) => match doctor() {
                Ok(report) => {
                    println!("Installed and verified Chrome native messaging host.");
                    print_doctor_report(&report);
                    println!("  extension: {}", chrome::CHROME_EXTENSION_ID);
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
                println!("Chrome native messaging host: healthy");
                print_doctor_report(&report);
                ExitCode::SUCCESS
            }
            Err(error) => fail(format!("Chrome native host is not healthy: {error}")),
        },
        [option] if option == "--uninstall" => match chrome::uninstall_native_host() {
            Ok(path) => {
                println!("Removed Chrome native messaging host registration.");
                println!("  manifest: {}", path.display());
                ExitCode::SUCCESS
            }
            Err(error) => fail(error.to_string()),
        },
        [option] if option == "--status" => match chrome::load_recent_chrome_state() {
            Ok(Some(snapshot)) => {
                println!("Chrome adapter: live");
                println!("  windows: {}", snapshot.windows.len());
                println!("  tabs: {}", snapshot.tab_count());
                println!("  extension: {}", snapshot.extension_version);
                ExitCode::SUCCESS
            }
            Ok(None) => {
                println!("Chrome adapter: no recent state");
                ExitCode::SUCCESS
            }
            Err(error) => fail(error.to_string()),
        },
        [option] if option == "-h" || option == "--help" => {
            print_usage();
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("error: invalid Chrome native-host arguments: {arguments:?}\n");
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn run_protocol() -> ExitCode {
    match chrome::run_native_host() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Context Capsule Chrome native host failed: {error}");
            ExitCode::from(1)
        }
    }
}

fn extension_origin() -> String {
    format!("chrome-extension://{}/", chrome::CHROME_EXTENSION_ID)
}

fn is_native_messaging_invocation(arguments: &[String]) -> bool {
    if arguments.is_empty() || arguments.len() > 2 || arguments[0] != extension_origin() {
        return false;
    }
    arguments
        .get(1)
        .is_none_or(|argument| argument.starts_with("--parent-window="))
}

#[derive(Debug)]
struct DoctorReport {
    manifest_path: PathBuf,
    executable_path: PathBuf,
    #[cfg(windows)]
    registry_manifest_path: PathBuf,
}

fn doctor() -> Result<DoctorReport, String> {
    let manifest_path = chrome::native_manifest_path().map_err(|error| error.to_string())?;
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

    require_string(&manifest, "name", chrome::NATIVE_HOST_NAME)?;
    require_string(&manifest, "type", "stdio")?;

    let allowed = manifest
        .get("allowed_origins")
        .and_then(Value::as_array)
        .ok_or_else(|| "manifest has no allowed_origins array".to_owned())?;
    let expected_origin = extension_origin();
    if allowed.len() != 1 || allowed[0].as_str() != Some(expected_origin.as_str()) {
        return Err(format!("manifest must authorize only '{expected_origin}'"));
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

    probe_native_host(&executable_path)?;

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

fn probe_native_host(executable: &Path) -> Result<(), String> {
    let mut command = Command::new(executable);
    command.arg(extension_origin());
    #[cfg(windows)]
    command.arg("--parent-window=0");

    let mut child = command
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
        "protocol_version": chrome::NATIVE_PROTOCOL_VERSION,
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
        r"HKCU\Software\Google\Chrome\NativeMessagingHosts\{}",
        chrome::NATIVE_HOST_NAME
    );
    let output = Command::new("reg.exe")
        .args(["query", &key, "/ve"])
        .output()
        .map_err(|error| format!("cannot query Chrome native-host registry key: {error}"))?;
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
        "  registry: HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts\\{} -> {}",
        chrome::NATIVE_HOST_NAME,
        report.registry_manifest_path.display()
    );
    println!("  browser-style protocol ping: ok");
}

fn fail(message: String) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(1)
}

fn print_usage() {
    println!("Context Capsule Chrome native host");
    println!();
    println!("Usage:");
    println!("  capsule-chrome-host --install");
    println!("  capsule-chrome-host --doctor");
    println!("  capsule-chrome-host --status");
    println!("  capsule-chrome-host --uninstall");
    println!();
    println!("--install writes the Chrome native manifest, registers it, and validates it.");
    println!(
        "--doctor verifies the manifest, executable, Windows registration, and protocol launch."
    );
}

#[cfg(test)]
mod tests {
    use super::{extension_origin, is_native_messaging_invocation};

    #[test]
    fn recognizes_chrome_browser_invocation_arguments() {
        assert!(is_native_messaging_invocation(&[extension_origin()]));
        assert!(is_native_messaging_invocation(&[
            extension_origin(),
            "--parent-window=0".to_owned(),
        ]));
    }

    #[test]
    fn rejects_wrong_origin_or_extra_arguments() {
        assert!(!is_native_messaging_invocation(&[
            "chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/".to_owned()
        ]));
        assert!(!is_native_messaging_invocation(&[
            extension_origin(),
            "--unexpected".to_owned(),
        ]));
        assert!(!is_native_messaging_invocation(&[
            extension_origin(),
            "--parent-window=0".to_owned(),
            "extra".to_owned(),
        ]));
    }

    #[cfg(windows)]
    #[test]
    fn parses_windows_registry_default_value() {
        let output = r#"
HKEY_CURRENT_USER\Software\Google\Chrome\NativeMessagingHosts\com.contextcapsule.chrome
    (Default)    REG_SZ    C:\Users\test\AppData\Local\ContextCapsule\native-messaging\com.contextcapsule.chrome.json
"#;
        assert_eq!(
            super::parse_registry_default(output).as_deref(),
            Some(
                r"C:\Users\test\AppData\Local\ContextCapsule\native-messaging\com.contextcapsule.chrome.json"
            )
        );
    }
}
