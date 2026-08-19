use crate::{
    diagnostics::{self, DoctorStatus},
    diff::{self, DiffChange, DiffKind},
    discovery, logging,
    persistence::{CapsuleStore, PersistenceError, parse_capsule_reference},
    snapshot,
};
use std::process::ExitCode;

pub fn update(arguments: Vec<String>) -> ExitCode {
    let name = match parse_single_name("update", arguments) {
        Ok(name) => name,
        Err(error) => return usage_error(error),
    };
    let reference = match parse_capsule_reference(&name) {
        Ok(reference) if reference.revision.is_none() => reference,
        Ok(_) => return usage_error("update expects a capsule name without @revision".to_owned()),
        Err(error) => return usage_error(error.to_string()),
    };

    let mut store = match CapsuleStore::open_default() {
        Ok(store) => store,
        Err(error) => return command_error(error.to_string()),
    };
    if let Err(error) = store.history(&reference.name) {
        return command_error(error.to_string());
    }

    println!("Discovering workspace for capsule '{}'...", reference.name);
    let discovery = match discovery::discover(true, true, true, true) {
        Ok(snapshot) => snapshot,
        Err(error) => return command_error(format!("discovery failed: {error}")),
    };
    let stored = match snapshot::capture_snapshot(&discovery) {
        Ok(snapshot) => snapshot,
        Err(error) => return command_error(error.to_string()),
    };
    let summary = match store.save(&reference.name, &stored, true) {
        Ok(summary) => summary,
        Err(error) => return command_error(error.to_string()),
    };

    println!(
        "Updated capsule '{}' -> revision {}.",
        summary.name, summary.current_revision
    );
    println!("  revisions retained: {}", summary.revision_count);
    println!(
        "  applications: {}",
        discovery
            .desktop
            .as_ref()
            .map(|desktop| desktop.applications.len())
            .unwrap_or(0)
    );
    println!("  developer tools: {}", discovery.tools.len());
    println!("  terminal sessions: {}", discovery.terminals.session_count());
    println!("  running containers: {}", discovery.docker.running_container_count());
    logging::info(
        "cli",
        format!(
            "capsule update completed; revision={} revisions={} applications={} terminals={} containers={}",
            summary.current_revision,
            summary.revision_count,
            discovery
                .desktop
                .as_ref()
                .map(|desktop| desktop.applications.len())
                .unwrap_or(0),
            discovery.terminals.session_count(),
            discovery.docker.running_container_count()
        ),
    );
    ExitCode::SUCCESS
}

pub fn history(arguments: Vec<String>) -> ExitCode {
    let name = match parse_single_name("history", arguments) {
        Ok(name) => name,
        Err(error) => return usage_error(error),
    };
    let store = match CapsuleStore::open_default() {
        Ok(store) => store,
        Err(error) => return command_error(error.to_string()),
    };
    let history = match store.history(&name) {
        Ok(history) => history,
        Err(error) => return command_error(error.to_string()),
    };

    println!("Capsule history: {name}");
    for revision in history {
        let current = if revision.current { " current" } else { "" };
        println!(
            "  {}@{}  [schema {}, captured {}]{}",
            revision.name,
            revision.revision,
            revision.schema_version,
            revision.created_at_unix_ms,
            current
        );
    }
    ExitCode::SUCCESS
}

pub fn diff(arguments: Vec<String>) -> ExitCode {
    let (before_ref, after_ref, json) = match parse_diff_arguments(arguments) {
        Ok(parsed) => parsed,
        Err(error) => return usage_error(error),
    };
    let store = match CapsuleStore::open_default() {
        Ok(store) => store,
        Err(error) => return command_error(error.to_string()),
    };
    let before = match store.load(&before_ref) {
        Ok(snapshot) => snapshot,
        Err(error) => return command_error(error.to_string()),
    };
    let after = match store.load(&after_ref) {
        Ok(snapshot) => snapshot,
        Err(error) => return command_error(error.to_string()),
    };
    let report = diff::diff_snapshots(&before, &after);

    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(output) => println!("{output}"),
            Err(error) => return command_error(format!("failed to render diff: {error}")),
        }
    } else {
        println!("Capsule diff: {before_ref} -> {after_ref}");
        if report.is_empty() {
            println!("  No semantic changes.");
        } else {
            for section in &report.sections {
                println!("\n{}", section.name);
                for change in &section.changes {
                    print_change(change);
                }
            }
            println!("\n{} semantic change(s).", report.change_count());
        }
    }

    logging::info(
        "cli",
        format!(
            "capsule diff completed; changes={} sections={}",
            report.change_count(),
            report.sections.len()
        ),
    );
    ExitCode::SUCCESS
}

pub fn doctor(arguments: Vec<String>) -> ExitCode {
    let (verbose, json) = match parse_doctor_arguments(arguments) {
        Ok(parsed) => parsed,
        Err(error) => return usage_error(error),
    };
    let report = diagnostics::run();

    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(output) => println!("{output}"),
            Err(error) => return command_error(format!("failed to render doctor report: {error}")),
        }
    } else {
        println!("Context Capsule Doctor");
        println!("Version {}", report.version);
        for check in &report.checks {
            let marker = match check.status {
                DoctorStatus::Ok => "OK",
                DoctorStatus::Warning => "WARN",
                DoctorStatus::Error => "ERROR",
            };
            println!("\n[{marker}] {} — {}", check.component, check.summary);
            if verbose {
                for detail in &check.details {
                    println!("       {detail}");
                }
            }
            if let Some(hint) = check.hint.as_deref() {
                if verbose || check.status != DoctorStatus::Ok {
                    println!("       hint: {hint}");
                }
            }
        }
        let errors = report
            .checks
            .iter()
            .filter(|check| check.status == DoctorStatus::Error)
            .count();
        println!(
            "\nSummary: {} error(s), {} warning(s).",
            errors,
            report.warning_count()
        );
    }

    let errors = report
        .checks
        .iter()
        .filter(|check| check.status == DoctorStatus::Error)
        .count();
    logging::info(
        "cli",
        format!(
            "doctor completed; checks={} errors={} warnings={}",
            report.checks.len(),
            errors,
            report.warning_count()
        ),
    );
    if report.has_errors() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn print_change(change: &DiffChange) {
    match change.kind {
        DiffKind::Added => println!(
            "  + {}: {}",
            change.key,
            change.after.as_deref().unwrap_or("(present)")
        ),
        DiffKind::Removed => println!(
            "  - {}: {}",
            change.key,
            change.before.as_deref().unwrap_or("(present)")
        ),
        DiffKind::Changed => println!(
            "  ~ {}: {} -> {}",
            change.key,
            change.before.as_deref().unwrap_or("(none)"),
            change.after.as_deref().unwrap_or("(none)")
        ),
    }
}

fn parse_single_name(command: &str, arguments: Vec<String>) -> Result<String, String> {
    match arguments.as_slice() {
        [name] if !name.starts_with('-') => Ok(name.clone()),
        _ => Err(format!("usage: capsule {command} <name>")),
    }
}

fn parse_diff_arguments(arguments: Vec<String>) -> Result<(String, String, bool), String> {
    let mut references = Vec::new();
    let mut json = false;
    for argument in arguments {
        match argument.as_str() {
            "--json" => json = true,
            value if value.starts_with('-') => {
                return Err(format!("unknown diff option '{value}'"));
            }
            value => references.push(value.to_owned()),
        }
    }
    match references.as_slice() {
        [before, after] => Ok((before.clone(), after.clone(), json)),
        _ => Err("usage: capsule diff <before> <after> [--json]".to_owned()),
    }
}

fn parse_doctor_arguments(arguments: Vec<String>) -> Result<(bool, bool), String> {
    let mut verbose = false;
    let mut json = false;
    for argument in arguments {
        match argument.as_str() {
            "--verbose" | "-v" => verbose = true,
            "--json" => json = true,
            other => return Err(format!("unknown doctor option '{other}'")),
        }
    }
    Ok((verbose, json))
}

fn usage_error(error: String) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::from(2)
}

fn command_error(error: String) -> ExitCode {
    logging::error("cli", format!("command failed: {error}"));
    eprintln!("error: {error}");
    ExitCode::from(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_parser_accepts_json_in_any_position() {
        assert_eq!(
            parse_diff_arguments(vec![
                "demo@1".to_owned(),
                "--json".to_owned(),
                "demo@2".to_owned(),
            ])
            .unwrap(),
            ("demo@1".to_owned(), "demo@2".to_owned(), true)
        );
        assert!(parse_diff_arguments(vec!["only-one".to_owned()]).is_err());
        assert!(parse_diff_arguments(vec![
            "a".to_owned(),
            "b".to_owned(),
            "c".to_owned()
        ])
        .is_err());
    }

    #[test]
    fn doctor_parser_is_strict() {
        assert_eq!(
            parse_doctor_arguments(vec!["--verbose".to_owned(), "--json".to_owned()]).unwrap(),
            (true, true)
        );
        assert!(parse_doctor_arguments(vec!["--bad".to_owned()]).is_err());
    }

    #[test]
    fn update_and_history_require_one_name() {
        assert_eq!(
            parse_single_name("update", vec!["demo".to_owned()]).unwrap(),
            "demo"
        );
        assert!(parse_single_name("update", Vec::new()).is_err());
        assert!(parse_single_name("history", vec!["a".to_owned(), "b".to_owned()]).is_err());
    }

    #[test]
    fn persistence_error_type_remains_available_to_lifecycle_module() {
        let error = PersistenceError::NotFound("demo".to_owned());
        assert!(error.to_string().contains("demo"));
    }
}
