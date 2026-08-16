use context_capsule::{
    orchestrator::{self, FullRestoreReport, SemanticAdapterReport},
    persistence::CapsuleStore,
    restore::RestoreReport,
};
use std::process::ExitCode;

pub fn run(arguments: Vec<String>) -> ExitCode {
    let (name, dry_run) = match parse_arguments(arguments) {
        Ok(parsed) => parsed,
        Err(error) => return usage_error(error),
    };

    let store = match CapsuleStore::open_default() {
        Ok(store) => store,
        Err(error) => return command_error(error.to_string()),
    };
    let stored = match store.load(&name) {
        Ok(snapshot) => snapshot,
        Err(error) => return command_error(error.to_string()),
    };

    if dry_run {
        println!("Planning full restore for capsule '{name}' (dry run)...");
    } else {
        println!("Restoring capsule '{name}'...");
    }

    let report = orchestrator::restore_capsule(&name, &stored, dry_run);
    print_report(&report, dry_run);

    if report.success() {
        if dry_run {
            println!("Dry run complete; no applications, semantic adapters, terminals, or containers were changed.");
        } else {
            println!("Restore complete.");
        }
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn print_report(report: &FullRestoreReport, dry_run: bool) {
    print_desktop("Desktop bootstrap", &report.initial_desktop, dry_run);
    print_semantic("Firefox", &report.firefox, dry_run);
    print_semantic("VS Code", &report.vscode, dry_run);

    println!("Docker:");
    println!("  resource groups in capsule: {}", report.docker.resources_total);
    println!("  already satisfied:          {}", report.docker.resources_already_satisfied);
    if dry_run {
        println!("  would restore:              {}", report.docker.resources_planned);
    } else {
        println!("  planned:                    {}", report.docker.resources_planned);
        println!("  restored:                   {}", report.docker.resources_restored);
    }
    for warning in &report.docker.warnings {
        println!("  warning: {warning}");
    }
    for failure in &report.docker.failures {
        eprintln!("  failed: {failure}");
    }

    println!("Terminals:");
    println!("  sessions in capsule:        {}", report.terminals.sessions_total);
    println!("  already satisfied:          {}", report.terminals.sessions_already_satisfied);
    println!("  VS Code/Cursor delegated:   {}", report.terminals.sessions_delegated);
    if dry_run {
        println!("  would launch sessions:      {}", report.terminals.sessions_planned);
        println!("  would restore WT layouts:   {}", report.terminals.layouts_planned);
    } else {
        println!("  launched sessions:          {}", report.terminals.sessions_launched);
        println!("  Windows Terminal layouts:   {}/{} restored", report.terminals.layouts_launched, report.terminals.layouts_total);
    }
    if report.terminals.sessions_unrestorable > 0 {
        println!("  safely unrestorable:        {}", report.terminals.sessions_unrestorable);
    }
    for warning in &report.terminals.warnings {
        println!("  warning: {warning}");
    }
    for failure in &report.terminals.failures {
        eprintln!("  failed: {failure}");
    }

    if let Some(final_desktop) = report.final_desktop.as_ref() {
        print_desktop("Final window reconciliation", final_desktop, false);
    }

    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    for failure in &report.failures {
        eprintln!("failed: {failure}");
    }
}

fn print_desktop(label: &str, report: &RestoreReport, dry_run: bool) {
    let desktop = &report.desktop;
    println!("{label}:");
    println!("  applications in capsule:   {}", desktop.applications_total);
    println!("  already running:           {}", desktop.applications_already_running);
    if dry_run {
        println!("  would launch:              {}", desktop.applications_planned_to_launch);
        println!("  windows already placed:    {}", desktop.windows_already_placed);
        println!("  windows to reposition:     {}", desktop.windows_planned_to_move);
    } else {
        println!("  launched:                  {}", desktop.applications_launched);
        println!("  windows already placed:    {}", desktop.windows_already_placed);
        println!("  windows repositioned:      {}", desktop.windows_moved);
    }
    if desktop.windows_missing > 0 {
        println!("  saved windows not observed: {}", desktop.windows_missing);
    }
    for warning in report.warnings.iter().chain(desktop.warnings.iter()) {
        println!("  warning: {warning}");
    }
    for failure in report.failures.iter().chain(desktop.failures.iter()) {
        eprintln!("  failed: {failure}");
    }
}

fn print_semantic(label: &str, report: &SemanticAdapterReport, dry_run: bool) {
    println!("{label} semantic context:");
    if !report.present_in_capsule {
        println!("  not captured in this capsule");
        return;
    }
    if dry_run {
        println!("  would request convergent semantic restore");
    } else {
        println!("  queued:    {}", yes_no(report.queued));
        println!("  claimed:   {}", yes_no(report.claimed));
        println!("  completed: {}", yes_no(report.completed));
        if let Some(summary) = report.summary.as_deref() {
            println!("  {summary}");
        }
    }
    for warning in &report.warnings {
        println!("  warning: {warning}");
    }
    for failure in &report.failures {
        eprintln!("  failed: {failure}");
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn parse_arguments(arguments: Vec<String>) -> Result<(String, bool), String> {
    let mut name = None;
    let mut dry_run = false;

    for argument in arguments {
        match argument.as_str() {
            "--dry-run" => dry_run = true,
            value if value.starts_with('-') => {
                return Err(format!("unknown restore option '{value}'"));
            }
            value if name.is_none() => name = Some(value.to_owned()),
            value => return Err(format!("unexpected restore argument '{value}'")),
        }
    }

    name.map(|name| (name, dry_run))
        .ok_or_else(|| "usage: capsule restore <name> [--dry-run]".to_owned())
}

fn usage_error(error: String) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::from(2)
}

fn command_error(error: String) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::from(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_supports_dry_run_without_accepting_extra_arguments() {
        assert_eq!(
            parse_arguments(vec!["demo".to_owned(), "--dry-run".to_owned()]).unwrap(),
            ("demo".to_owned(), true)
        );
        assert_eq!(parse_arguments(vec!["demo".to_owned()]).unwrap(), ("demo".to_owned(), false));
        assert!(parse_arguments(Vec::new()).is_err());
        assert!(parse_arguments(vec!["demo".to_owned(), "--bad".to_owned()]).is_err());
        assert!(parse_arguments(vec!["one".to_owned(), "two".to_owned()]).is_err());
    }
}
