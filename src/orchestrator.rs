use crate::{
    adapters::{
        docker::{DockerSnapshot, DockerStatus},
        docker_restore::{self, ConvergentDockerRestoreReport},
        terminal::{TerminalSnapshot, TerminalStatus},
        terminal_restore::{self, TerminalRestoreReport},
    },
    persistence::StoredCapsuleSnapshot,
    restore::{self, RestoreOptions, RestoreReport},
    restore_bridge::{self, RestoreAdapter, RestoreTicket, RestoreTicketState},
};
use serde_json::Value;
use std::{thread, time::{Duration, Instant}};

const SEMANTIC_CLAIM_TIMEOUT: Duration = Duration::from_secs(15);
const SEMANTIC_COMPLETION_TIMEOUT: Duration = Duration::from_secs(60);
const SEMANTIC_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SemanticAdapterReport {
    pub present_in_capsule: bool,
    pub planned: bool,
    pub queued: bool,
    pub claimed: bool,
    pub completed: bool,
    pub summary: Option<String>,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

impl SemanticAdapterReport {
    pub fn success(&self) -> bool {
        self.failures.is_empty()
    }
}

#[derive(Debug)]
pub struct FullRestoreReport {
    pub initial_desktop: RestoreReport,
    pub firefox: SemanticAdapterReport,
    pub vscode: SemanticAdapterReport,
    pub docker: ConvergentDockerRestoreReport,
    pub terminals: TerminalRestoreReport,
    pub final_desktop: Option<RestoreReport>,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

impl FullRestoreReport {
    pub fn success(&self) -> bool {
        self.failures.is_empty()
            && self.initial_desktop.success()
            && self.final_desktop.as_ref().is_none_or(RestoreReport::success)
            && self.firefox.success()
            && self.vscode.success()
            && self.docker.success()
            && self.terminals.success()
    }
}

pub fn restore_capsule(
    name: &str,
    stored: &StoredCapsuleSnapshot,
    dry_run: bool,
) -> FullRestoreReport {
    let initial_desktop = restore::restore_snapshot(&stored.snapshot, RestoreOptions { dry_run });
    let mut warnings = Vec::new();
    let mut failures = Vec::new();

    let firefox_present = section_present(&stored.snapshot, "/browsers/firefox");
    let vscode_present = section_present(&stored.snapshot, "/editors/vscode");

    let (firefox_ticket, mut firefox) = semantic_plan(
        RestoreAdapter::Firefox,
        name,
        firefox_present,
        dry_run,
    );
    let (vscode_ticket, mut vscode) = semantic_plan(
        RestoreAdapter::VsCode,
        name,
        vscode_present,
        dry_run,
    );

    let firefox_waiter = firefox_ticket.map(|ticket| thread::spawn(move || wait_for_semantic(ticket)));
    let vscode_waiter = vscode_ticket.map(|ticket| thread::spawn(move || wait_for_semantic(ticket)));

    let docker_snapshot = match docker_snapshot(stored) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            failures.push(error.clone());
            DockerSnapshot::unavailable(error)
        }
    };
    let terminal_snapshot = match terminal_snapshot(stored) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            failures.push(error.clone());
            TerminalSnapshot {
                status: TerminalStatus::Unsupported,
                message: Some(error),
                windows_terminal_layouts: Vec::new(),
                sessions: Vec::new(),
                warnings: Vec::new(),
                history: crate::adapters::terminal::TerminalHistoryPolicy {
                    captured: false,
                    reason: "terminal snapshot could not be parsed".to_owned(),
                },
            }
        }
    };

    let docker_worker = if dry_run {
        None
    } else {
        let snapshot = docker_snapshot.clone();
        Some(thread::spawn(move || docker_restore::restore(&snapshot, false)))
    };
    let docker = if dry_run {
        docker_restore::restore(&docker_snapshot, true)
    } else {
        ConvergentDockerRestoreReport::default()
    };

    let terminals = terminal_restore::restore(&terminal_snapshot, dry_run);

    let docker = match docker_worker {
        Some(worker) => match worker.join() {
            Ok(report) => report,
            Err(_) => {
                let mut report = docker;
                report.failures.push("Docker restore worker panicked".to_owned());
                report
            }
        },
        None => docker,
    };

    if let Some(waiter) = firefox_waiter {
        match waiter.join() {
            Ok(report) => firefox = report,
            Err(_) => firefox.failures.push("Firefox restore waiter panicked".to_owned()),
        }
    }
    if let Some(waiter) = vscode_waiter {
        match waiter.join() {
            Ok(report) => vscode = report,
            Err(_) => vscode.failures.push("VS Code restore waiter panicked".to_owned()),
        }
    }

    let final_desktop = if dry_run {
        None
    } else {
        Some(restore::restore_snapshot(
            &stored.snapshot,
            RestoreOptions { dry_run: false },
        ))
    };

    if !matches!(docker_snapshot.status, DockerStatus::Available)
        && docker_snapshot.running_container_count() > 0
    {
        warnings.push("Docker resources were saved, but Docker was not captured as available".to_owned());
    }

    FullRestoreReport {
        initial_desktop,
        firefox,
        vscode,
        docker,
        terminals,
        final_desktop,
        warnings,
        failures,
    }
}

fn semantic_plan(
    adapter: RestoreAdapter,
    capsule_name: &str,
    present: bool,
    dry_run: bool,
) -> (Option<RestoreTicket>, SemanticAdapterReport) {
    let mut report = SemanticAdapterReport {
        present_in_capsule: present,
        planned: present,
        ..SemanticAdapterReport::default()
    };
    if !present || dry_run {
        return (None, report);
    }

    match restore_bridge::queue_restore(adapter, capsule_name) {
        Ok(ticket) => {
            report.queued = true;
            (Some(ticket), report)
        }
        Err(error) => {
            report.failures.push(format!(
                "could not queue {} semantic restore: {error}",
                adapter.as_str()
            ));
            (None, report)
        }
    }
}

fn wait_for_semantic(ticket: RestoreTicket) -> SemanticAdapterReport {
    let mut report = SemanticAdapterReport {
        present_in_capsule: true,
        planned: true,
        queued: true,
        ..SemanticAdapterReport::default()
    };
    let started = Instant::now();
    let mut claimed_at = None;

    loop {
        match ticket.state() {
            Ok(RestoreTicketState::Completed) => {
                report.completed = true;
                match ticket.read_result() {
                    Ok(Some(result)) => {
                        if result.ok {
                            report.summary = result.summary;
                        } else {
                            report.failures.push(result.error.unwrap_or_else(|| {
                                format!("{} semantic restore failed", ticket.adapter.as_str())
                            }));
                        }
                    }
                    Ok(None) => report.failures.push(format!(
                        "{} restore completed without a result payload",
                        ticket.adapter.as_str()
                    )),
                    Err(error) => report.failures.push(format!(
                        "could not read {} restore result: {error}",
                        ticket.adapter.as_str()
                    )),
                }
                ticket.cleanup();
                return report;
            }
            Ok(RestoreTicketState::Claimed) => {
                report.claimed = true;
                claimed_at.get_or_insert_with(Instant::now);
            }
            Ok(RestoreTicketState::Pending) => {
                if started.elapsed() >= SEMANTIC_CLAIM_TIMEOUT {
                    match ticket.cancel_pending() {
                        Ok(true) => {
                            report.warnings.push(format!(
                                "{} semantic adapter did not claim the restore request within {} seconds; the app/extension may be unavailable",
                                ticket.adapter.as_str(),
                                SEMANTIC_CLAIM_TIMEOUT.as_secs()
                            ));
                            ticket.cleanup();
                            return report;
                        }
                        Ok(false) => {}
                        Err(error) => {
                            report.failures.push(format!(
                                "could not cancel unclaimed {} restore request: {error}",
                                ticket.adapter.as_str()
                            ));
                            ticket.cleanup();
                            return report;
                        }
                    }
                }
            }
            Ok(RestoreTicketState::Missing) => {
                report.failures.push(format!(
                    "{} restore request disappeared before completion",
                    ticket.adapter.as_str()
                ));
                ticket.cleanup();
                return report;
            }
            Err(error) => {
                report.failures.push(format!(
                    "could not inspect {} restore request: {error}",
                    ticket.adapter.as_str()
                ));
                ticket.cleanup();
                return report;
            }
        }

        if claimed_at.is_some_and(|time: Instant| time.elapsed() >= SEMANTIC_COMPLETION_TIMEOUT) {
            report.failures.push(format!(
                "{} semantic restore did not complete within {} seconds after being claimed",
                ticket.adapter.as_str(),
                SEMANTIC_COMPLETION_TIMEOUT.as_secs()
            ));
            return report;
        }
        thread::sleep(SEMANTIC_POLL_INTERVAL);
    }
}

fn section_present(snapshot: &Value, pointer: &str) -> bool {
    snapshot.pointer(pointer).is_some_and(|value| !value.is_null())
}

fn docker_snapshot(stored: &StoredCapsuleSnapshot) -> Result<DockerSnapshot, String> {
    stored.docker().map_err(|error| error.to_string())
}

fn terminal_snapshot(stored: &StoredCapsuleSnapshot) -> Result<TerminalSnapshot, String> {
    let value = stored
        .snapshot
        .get("terminals")
        .cloned()
        .ok_or_else(|| "snapshot has no terminal section".to_owned())?;
    serde_json::from_value(value).map_err(|error| format!("invalid terminal snapshot: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn semantic_sections_are_planned_only_when_present() {
        let snapshot = json!({
            "browsers": { "firefox": { "schema_version": 1 } },
            "editors": { "vscode": null }
        });
        assert!(section_present(&snapshot, "/browsers/firefox"));
        assert!(!section_present(&snapshot, "/editors/vscode"));
        assert!(!section_present(&snapshot, "/missing"));
    }

    #[test]
    fn dry_run_semantic_plan_never_writes_request() {
        let (ticket, report) = semantic_plan(RestoreAdapter::Firefox, "demo", true, true);
        assert!(ticket.is_none());
        assert!(report.planned);
        assert!(!report.queued);
    }
}
