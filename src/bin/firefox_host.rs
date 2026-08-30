use context_capsule::browser;
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();

    if arguments.is_empty() || is_native_messaging_invocation(&arguments) {
        return run_protocol();
    }

    match arguments.as_slice() {
        [option] if option == "--install" => match install_native_host() {
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
            Err(error) => fail(error),
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

fn install_native_host() -> Result<PathBuf, String> {
    let manifest_path = browser::install_native_host().map_err(|error| error.to_string())?;

    #[cfg(windows)]
    pin_windows_native_host_executable(&manifest_path)?;

    Ok(manifest_path)
}

#[cfg(windows)]
fn pin_windows_native_host_executable(manifest_path: &Path) -> Result<PathBuf, String> {
    let source = env::current_exe()
        .map_err(|error| format!("cannot locate the native-host executable: {error}"))?;
    if !source.is_file() {
        return Err(format!(
            "native-host executable does not exist at '{}'",
            source.display()
        ));
    }

    let local_app_data = env::var_os("LOCALAPPDATA")
        .ok_or_else(|| "LOCALAPPDATA is not available".to_owned())?;
    let install_dir = PathBuf::from(local_app_data)
        .join("ContextCapsule")
        .join("native-messaging")
        .join("bin");
    fs::create_dir_all(&install_dir).map_err(|error| {
        format!(
            "cannot create native-host install directory '{}': {error}",
            install_dir.display()
        )
    })?;

    // Never point Firefox at target/debug or target/release. Cargo clean and
    // branch switching are normal development operations and must not make an
    // already-installed browser extension lose its native host. Use a unique
    // filename so reinstalling also works while an older host process is still
    // open and therefore locked by Windows.
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let installed = install_dir.join(format!(
        "capsule-firefox-host-{}-{nonce}.exe",
        std::process::id()
    ));
    fs::copy(&source, &installed).map_err(|error| {
        format!(
            "cannot copy native host from '{}' to '{}': {error}",
            source.display(),
            installed.display()
        )
    })?;

    let manifest_bytes = fs::read(manifest_path).map_err(|error| {
        format!(
            "cannot read native manifest '{}': {error}",
            manifest_path.display()
        )
    })?;
    let mut manifest: Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| format!("native manifest is invalid JSON: {error}"))?;
    let object = manifest
        .as_object_mut()
        .ok_or_else(|| "native manifest root is not an object".to_owned())?;
    object.insert(
        "path".to_owned(),
        Value::String(installed.to_string_lossy().to_string()),
    );
    fs::write(
        manifest_path,
        serde_json::to_vec_pretty(&manifest)
            .map_err(|error| format!("cannot encode native manifest: {error}"))?,
    )
    .map_err(|error| {
        format!(
            "cannot update native manifest '{}': {error}",
            manifest_path.display()
        )
    })?;

    cleanup_stale_windows_host_copies(&install_dir, &installed);
    Ok(installed)
}

#[cfg(windows)]
fn cleanup_stale_windows_host_copies(directory: &Path, keep: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if same_windows_path(&path, keep) {
            continue;
        }
        let is_host_copy = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with("capsule-firefox-host-")
                    && name.to_ascii_lowercase().ends_with(".exe")
            });
        if is_host_copy {
            // An older host may still be serving an open Zen/Firefox instance,
            // in which case Windows keeps the executable locked. Leaving that
            // one file until a later reinstall is harmless.
            let _ = fs::remove_file(path);
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

fn doctor_probe_arguments() -> Vec<String> {
    // The host intentionally supports zero-argument protocol mode for local
    // diagnostics. Browser liveness, however, is published only for Firefox's
    // exact manifest+extension invocation. Keeping doctor on zero arguments
    // proves the executable/protocol without fabricating a browser connection.
    Vec::new()
}

fn probe_native_host(executable: &Path) -> Result<(), String> {
    let mut child = Command::new(executable)
        .args(doctor_probe_arguments())
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
    println!("  diagnostic protocol ping: ok");
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
    println!(
        "--install copies the host to a durable per-user location, writes and registers the native manifest, and validates the result."
    );
    println!(
        "--doctor verifies the manifest, executable, Windows registration, and native protocol without publishing browser liveness."
    );
    println!();
    println!(
        "Firefox/Zen launches this executable with the native manifest path and extension ID; only that browser-style invocation publishes a live browser session."
    );
}

#[cfg(test)]
mod tests {
    use super::{doctor_probe_arguments, is_native_messaging_invocation};
    use context_capsule::browser;

    #[test]
    fn recognizes_firefox_browser_invocation_arguments() {
        let arguments = vec![
            format!("{}.json", browser::NATIVE_HOST_NAME),
            browser::FIREFOX_EXTENSION_ID.to_owned(),
        ];
        assert!(is_native_messaging_invocation(&arguments));
    }

    #[test]
    fn rejects_wrong_extension_or_manifest_name() {
        let wrong_extension = vec![
            format!("{}.json", browser::NATIVE_HOST_NAME),
            "other@example.test".to_owned(),
        ];
        assert!(!is_native_messaging_invocation(&wrong_extension));

        let wrong_manifest = vec![
            "other-host.json".to_owned(),
            browser::FIREFOX_EXTENSION_ID.to_owned(),
        ];
        assert!(!is_native_messaging_invocation(&wrong_manifest));
    }

    #[test]
    fn doctor_probe_never_uses_browser_liveness_arguments() {
        assert!(doctor_probe_arguments().is_empty());
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
