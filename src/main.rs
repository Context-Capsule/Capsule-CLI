mod desktop;
mod discovery;
mod git;
mod system;
mod toolchain;

use crate::{
    desktop::{ApplicationInfo, DesktopSnapshot, IgnoredCandidate, WindowInfo},
    discovery::{DiscoverySnapshot, GitState},
};
use std::{env, process::ExitCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InspectSection {
    All,
    Apps,
    Windows,
    Processes,
    Ignored,
    Tools,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InspectOptions {
    section: InspectSection,
    verbose: bool,
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);

    match args.next().as_deref() {
        Some("inspect") => {
            let remaining = args.collect::<Vec<_>>();
            if remaining
                .iter()
                .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
            {
                print_inspect_usage();
                return ExitCode::SUCCESS;
            }

            match parse_inspect_options(remaining) {
                Ok(options) => inspect(options),
                Err(error) => {
                    eprintln!("error: {error}\n");
                    print_usage();
                    ExitCode::from(2)
                }
            }
        }
        Some("apps") => {
            if args.next().is_some() {
                eprintln!("error: 'apps' does not accept arguments\n");
                print_usage();
                ExitCode::from(2)
            } else {
                inspect(InspectOptions {
                    section: InspectSection::Apps,
                    verbose: true,
                })
            }
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

fn parse_inspect_options<I>(args: I) -> Result<InspectOptions, String>
where
    I: IntoIterator<Item = String>,
{
    let mut options = InspectOptions {
        section: InspectSection::All,
        verbose: false,
    };
    let mut selected_section = false;

    for argument in args {
        match argument.as_str() {
            "--verbose" | "-v" => options.verbose = true,
            "--apps" => set_section(&mut options, &mut selected_section, InspectSection::Apps)?,
            "--windows" => {
                set_section(&mut options, &mut selected_section, InspectSection::Windows)?
            }
            "--processes" => set_section(
                &mut options,
                &mut selected_section,
                InspectSection::Processes,
            )?,
            "--ignored" => {
                set_section(&mut options, &mut selected_section, InspectSection::Ignored)?
            }
            "--tools" => set_section(&mut options, &mut selected_section, InspectSection::Tools)?,
            other => return Err(format!("unknown inspect option '{other}'")),
        }
    }

    Ok(options)
}

fn set_section(
    options: &mut InspectOptions,
    selected: &mut bool,
    section: InspectSection,
) -> Result<(), String> {
    if *selected {
        return Err(
            "choose at most one of --apps, --windows, --processes, --ignored, or --tools"
                .to_owned(),
        );
    }

    options.section = section;
    *selected = true;
    Ok(())
}

fn inspect(options: InspectOptions) -> ExitCode {
    let include_tools = matches!(options.section, InspectSection::All | InspectSection::Tools);
    let include_desktop = !matches!(options.section, InspectSection::Tools);

    let snapshot = match discovery::discover(include_tools, include_desktop) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("Discovery failed: {error}");
            return ExitCode::from(1);
        }
    };

    match options.section {
        InspectSection::All => print_full_snapshot(&snapshot, options.verbose),
        InspectSection::Tools => print_tools(&snapshot, true),
        InspectSection::Apps => print_desktop_apps(&snapshot, true),
        InspectSection::Windows => print_windows(&snapshot),
        InspectSection::Processes => print_process_candidates(&snapshot),
        InspectSection::Ignored => print_ignored(&snapshot),
    }

    ExitCode::SUCCESS
}

fn print_full_snapshot(snapshot: &DiscoverySnapshot, verbose: bool) {
    println!("Context Capsule discovery\n");
    print_working_context(snapshot, verbose);
    println!();
    print_tools(snapshot, verbose);
    println!();

    match snapshot.desktop.as_ref() {
        Ok(desktop) => {
            print_displays(desktop, verbose);
            println!();
            print_applications(desktop, verbose);

            let virtual_desktops = desktop.virtual_desktops();
            if !virtual_desktops.is_empty() {
                println!("\nVirtual desktops observed: {}", virtual_desktops.len());
                for (id, current, window_count) in virtual_desktops {
                    let current_label = match current {
                        Some(true) => "current",
                        Some(false) => "not current",
                        None => "current state unknown",
                    };
                    println!("  {id} ({current_label}, {window_count} captured window(s))");
                }
            }

            if verbose && !desktop.ignored.is_empty() {
                println!();
                print_ignored_desktop(desktop);
            }
        }
        Err(error) => println!("Desktop: {error}"),
    }
}

fn print_working_context(snapshot: &DiscoverySnapshot, verbose: bool) {
    println!("Working context");
    println!(
        "  System:            {} {} ({})",
        snapshot.system.platform,
        snapshot
            .system
            .version
            .as_deref()
            .unwrap_or("version unknown"),
        snapshot.system.architecture
    );
    println!(
        "  Current directory: {}",
        snapshot.current_directory.display()
    );

    match &snapshot.git {
        GitState::Context(context) => {
            println!("  Git repository:    {}", context.repository_root);
            println!(
                "  Git branch:        {}",
                context.branch.as_deref().unwrap_or("(detached HEAD)")
            );
            if let Some(head) = context.head.as_ref() {
                println!("  Git HEAD:          {head}");
            }
            println!(
                "  Git working tree:  {}",
                if context.dirty { "dirty" } else { "clean" }
            );

            if verbose {
                if let Some(remote) = context.remote_origin.as_ref() {
                    println!("  Git origin:        {remote}");
                }
                println!("  Git stashes:       {}", context.stash_count);
                if !context.changed_files.is_empty() {
                    println!("  Changed files:");
                    for path in &context.changed_files {
                        println!("    - {path}");
                    }
                }
            }
        }
        GitState::NotRepository => println!("  Git repository:    not detected"),
        GitState::GitUnavailable => {
            println!("  Git repository:    unavailable (git not installed)")
        }
    }
}

fn print_tools(snapshot: &DiscoverySnapshot, verbose: bool) {
    if snapshot.tools.is_empty() {
        println!("Developer tools: none detected");
    } else {
        println!("Developer tools: {}", snapshot.tools.len());
        for tool in &snapshot.tools {
            println!("  {:<16} {}", tool.name, tool.version);
            if verbose {
                println!("    command: {}", tool.command);
                if let Some(path) = tool.executable_path.as_ref() {
                    println!("    path:    {path}");
                }
            }
        }
    }

    if !snapshot.version_hints.is_empty() {
        println!("\nProject version pins: {}", snapshot.version_hints.len());
        for hint in &snapshot.version_hints {
            println!("  {}: {}", hint.source, compact_hint(&hint.value));
        }
    }
}

fn compact_hint(value: &str) -> String {
    const MAX_CHARS: usize = 200;

    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_CHARS {
        return compact;
    }

    let mut truncated = compact.chars().take(MAX_CHARS).collect::<String>();
    truncated.push('…');
    truncated
}

fn print_desktop_apps(snapshot: &DiscoverySnapshot, verbose: bool) {
    match snapshot.desktop.as_ref() {
        Ok(desktop) => {
            print_applications(desktop, verbose);
            if verbose {
                println!();
                print_displays(desktop, true);
            }
        }
        Err(error) => println!("Desktop: {error}"),
    }
}

fn print_applications(desktop: &DesktopSnapshot, verbose: bool) {
    if desktop.applications.is_empty() {
        println!("Applications: none detected");
        return;
    }

    println!("Applications: {}", desktop.applications.len());
    for application in &desktop.applications {
        print_application(application, verbose);
    }
}

fn print_application(application: &ApplicationInfo, verbose: bool) {
    println!(
        "  {} [{}%, {}]",
        application.name,
        application.confidence,
        application.classification.as_str()
    );

    if let Some(version) = application.file_version.as_ref() {
        println!("    version: {version}");
    }

    if let Some(launch) = application.launch.as_ref() {
        println!(
            "    launch:  {} -> {}",
            launch.strategy.as_str(),
            launch.target
        );
    } else {
        println!("    launch:  unavailable");
    }

    if application.discovered_as_background {
        println!("    state:   running in background/tray (known restorable app)");
    }

    if verbose {
        println!("    PID(s):  {:?}", application.pids);
        if let Some(parent) = application.parent_pid {
            println!("    parent:  PID {parent}");
        }
        if let Some(application_id) = application.app_user_model_id.as_ref() {
            println!("    AUMID:   {application_id}");
        }
        println!("    reason:  {}", application.classification_reason);
    }

    for window in &application.windows {
        print_window(window, verbose);
    }
}

fn print_window(window: &WindowInfo, verbose: bool) {
    let foreground = if window.is_foreground {
        " [foreground]"
    } else {
        ""
    };
    println!("    window:  {}{}", window.title, foreground);
    println!(
        "      placement: {} on {} ({}, {}% scale)",
        window.state, window.display_device, window.display_relation, window.display_scale_percent
    );

    if let Some(bounds) = window.normalized_bounds {
        println!(
            "      relative:  x={:.1}% y={:.1}% width={:.1}% height={:.1}%",
            bounds.x * 100.0,
            bounds.y * 100.0,
            bounds.width * 100.0,
            bounds.height * 100.0
        );
    }

    if let Some(desktop_id) = window.virtual_desktop_id.as_ref() {
        let current = match window.is_on_current_virtual_desktop {
            Some(true) => "current",
            Some(false) => "not current",
            None => "unknown",
        };
        println!("      desktop:   {desktop_id} ({current})");
    }

    if verbose {
        println!(
            "      pixels:    ({}, {}) -> ({}, {})",
            window.bounds.left, window.bounds.top, window.bounds.right, window.bounds.bottom
        );
        if let Some(restore) = window.restore_bounds {
            println!(
                "      restore:   ({}, {}) -> ({}, {})",
                restore.left, restore.top, restore.right, restore.bottom
            );
        }
        println!("      z-order:   {}", window.z_order);
        println!(
            "      taskbar:   {}",
            if window.taskbar_candidate {
                "candidate"
            } else {
                "no"
            }
        );
    }
}

fn print_displays(desktop: &DesktopSnapshot, verbose: bool) {
    if desktop.displays.is_empty() {
        println!("Displays: none detected");
        return;
    }

    println!("Displays: {}", desktop.displays.len());
    for display in &desktop.displays {
        println!(
            "  {} — {}x{}, {}, {}% scale, {}",
            display.device_name,
            display.bounds.width(),
            display.bounds.height(),
            display.relation_to_primary,
            display.scale_percent,
            display.orientation
        );

        if verbose {
            println!(
                "    desktop bounds: ({}, {}) -> ({}, {})",
                display.bounds.left,
                display.bounds.top,
                display.bounds.right,
                display.bounds.bottom
            );
            println!(
                "    work area:      ({}, {}) -> ({}, {})",
                display.work_area.left,
                display.work_area.top,
                display.work_area.right,
                display.work_area.bottom
            );
        }
    }
}

fn print_windows(snapshot: &DiscoverySnapshot) {
    match snapshot.desktop.as_ref() {
        Ok(desktop) => {
            let total = desktop
                .applications
                .iter()
                .map(|application| application.windows.len())
                .sum::<usize>();
            println!("Captured application windows: {total}");

            for application in &desktop.applications {
                for window in &application.windows {
                    println!("\n{} [PID {}]", application.name, application.primary_pid);
                    print_window(window, true);
                }
            }
        }
        Err(error) => println!("Desktop: {error}"),
    }
}

fn print_process_candidates(snapshot: &DiscoverySnapshot) {
    match snapshot.desktop.as_ref() {
        Ok(desktop) => {
            println!("Restorable application candidates");
            for application in &desktop.applications {
                println!(
                    "  {} {:?} -> {} ({}%, {})",
                    application.name,
                    application.pids,
                    application.classification.as_str(),
                    application.confidence,
                    application.classification_reason
                );
            }

            println!("\nIgnored/uncertain candidates");
            for candidate in &desktop.ignored {
                print_ignored_candidate(candidate);
            }
        }
        Err(error) => println!("Desktop: {error}"),
    }
}

fn print_ignored(snapshot: &DiscoverySnapshot) {
    match snapshot.desktop.as_ref() {
        Ok(desktop) => print_ignored_desktop(desktop),
        Err(error) => println!("Desktop: {error}"),
    }
}

fn print_ignored_desktop(desktop: &DesktopSnapshot) {
    if desktop.ignored.is_empty() {
        println!("Ignored candidates: none");
        return;
    }

    println!("Ignored candidates: {}", desktop.ignored.len());
    for candidate in &desktop.ignored {
        print_ignored_candidate(candidate);
    }
}

fn print_ignored_candidate(candidate: &IgnoredCandidate) {
    println!(
        "  {} [PID {}] -> {} ({}%)",
        candidate.executable,
        candidate.pid,
        candidate.classification.as_str(),
        candidate.confidence
    );
    if let Some(title) = candidate.window_title.as_ref() {
        println!("    window: {title}");
    }
    if let Some(parent) = candidate.parent_pid {
        println!("    parent: PID {parent}");
    }
    if let Some(path) = candidate.executable_path.as_ref() {
        println!("    path:   {path}");
    }
    println!("    reason: {}", candidate.reason);
}

fn print_usage() {
    println!("Context Capsule CLI\n");
    println!("Usage:");
    println!("  capsule inspect [options]");
    println!("  capsule apps\n");
    println!("Commands:");
    println!("  inspect    Discover current workspace, tools, applications, windows and displays");
    println!("  apps       Compatibility alias for 'capsule inspect --apps'");
    println!();
    print_inspect_usage();
}

fn print_inspect_usage() {
    println!("Inspect options:");
    println!("  -v, --verbose     Include paths, ignored candidates and raw fallback geometry");
    println!("      --apps        Show discovered restorable desktop applications");
    println!("      --windows     Show captured application windows and placement metadata");
    println!("      --processes   Show classification decisions for process/window candidates");
    println!("      --ignored     Show helper, shell and uncertain candidates that are not stored");
    println!("      --tools       Show detected developer runtimes and project version pins");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<InspectOptions, String> {
        parse_inspect_options(arguments.iter().map(|value| (*value).to_owned()))
    }

    #[test]
    fn inspect_defaults_to_all_sections() {
        assert_eq!(
            parse(&[]).expect("options"),
            InspectOptions {
                section: InspectSection::All,
                verbose: false
            }
        );
    }

    #[test]
    fn verbose_can_be_combined_with_one_section() {
        assert_eq!(
            parse(&["--windows", "--verbose"]).expect("options"),
            InspectOptions {
                section: InspectSection::Windows,
                verbose: true
            }
        );
    }

    #[test]
    fn multiple_section_filters_are_rejected() {
        assert!(parse(&["--apps", "--tools"]).is_err());
    }

    #[test]
    fn unknown_option_is_rejected() {
        assert!(parse(&["--definitely-not-real"]).is_err());
    }

    #[test]
    fn compact_hint_collapses_whitespace_and_limits_output() {
        assert_eq!(compact_hint("22.14.0\n"), "22.14.0");
        assert_eq!(compact_hint("a\n b\t c"), "a b c");

        let long = "x".repeat(250);
        let result = compact_hint(&long);
        assert_eq!(result.chars().count(), 201);
        assert!(result.ends_with('…'));
    }
}
