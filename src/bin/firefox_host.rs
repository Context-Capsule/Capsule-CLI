use context_capsule::browser;
use std::{env, process::ExitCode};

fn main() -> ExitCode {
    match env::args().nth(1).as_deref() {
        None => match browser::run_native_host() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Context Capsule Firefox native host failed: {error}");
                ExitCode::from(1)
            }
        },
        Some("--install") => match browser::install_native_host() {
            Ok(path) => {
                println!("Installed Firefox native messaging host.");
                println!("  manifest: {}", path.display());
                println!("  extension: {}", browser::FIREFOX_EXTENSION_ID);
                ExitCode::SUCCESS
            }
            Err(error) => fail(error.to_string()),
        },
        Some("--uninstall") => match browser::uninstall_native_host() {
            Ok(path) => {
                println!("Removed Firefox native messaging host registration.");
                println!("  manifest: {}", path.display());
                ExitCode::SUCCESS
            }
            Err(error) => fail(error.to_string()),
        },
        Some("--status") => match browser::load_recent_firefox_state() {
            Ok(Some(snapshot)) => {
                println!("Firefox adapter: live");
                println!("  windows: {}", snapshot.windows.len());
                println!("  tabs: {}", snapshot.tab_count());
                println!("  extension: {}", snapshot.extension_version);
                ExitCode::SUCCESS
            }
            Ok(None) => {
                println!("Firefox adapter: no recent state");
                ExitCode::SUCCESS
            }
            Err(error) => fail(error.to_string()),
        },
        Some("-h" | "--help") => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("error: unknown option '{other}'\n");
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn fail(message: String) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(1)
}

fn print_usage() {
    println!("Context Capsule Firefox native host");
    println!();
    println!("Usage:");
    println!("  capsule-firefox-host --install");
    println!("  capsule-firefox-host --status");
    println!("  capsule-firefox-host --uninstall");
    println!();
    println!("With no arguments the binary runs the Firefox native-messaging protocol on stdin/stdout.");
}
