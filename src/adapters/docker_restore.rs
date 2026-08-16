use super::docker::{
    self, ComposeProject, ContainerResource, DockerRestoreReport, DockerSnapshot, DockerStatus,
};
use std::collections::HashSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConvergentDockerRestoreReport {
    pub resources_total: usize,
    pub resources_already_satisfied: usize,
    pub resources_planned: usize,
    pub resources_restored: usize,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

impl ConvergentDockerRestoreReport {
    pub fn success(&self) -> bool {
        self.failures.is_empty()
    }
}

pub fn restore(snapshot: &DockerSnapshot, dry_run: bool) -> ConvergentDockerRestoreReport {
    let total = snapshot.compose_projects.len() + snapshot.standalone_containers.len();
    let mut report = ConvergentDockerRestoreReport {
        resources_total: total,
        ..ConvergentDockerRestoreReport::default()
    };

    if total == 0 {
        if !matches!(snapshot.status, DockerStatus::Available) {
            report.warnings.push(
                snapshot
                    .message
                    .clone()
                    .unwrap_or_else(|| "Docker was not captured as available".to_owned()),
            );
        }
        return report;
    }

    let current = docker::discover();
    let running = running_container_names(&current);
    let missing = missing_snapshot(snapshot, &running);
    report.resources_planned = missing.compose_projects.len() + missing.standalone_containers.len();
    report.resources_already_satisfied = total.saturating_sub(report.resources_planned);

    if dry_run || report.resources_planned == 0 {
        if !matches!(current.status, DockerStatus::Available) && report.resources_planned > 0 {
            report.warnings.push(
                current
                    .message
                    .clone()
                    .unwrap_or_else(|| "Docker is currently unavailable".to_owned()),
            );
        }
        return report;
    }

    let inner = docker::restore(&missing);
    apply_inner_report(&mut report, inner);
    report
}

fn missing_snapshot(snapshot: &DockerSnapshot, running: &HashSet<String>) -> DockerSnapshot {
    DockerSnapshot {
        status: snapshot.status.clone(),
        context: snapshot.context.clone(),
        message: snapshot.message.clone(),
        compose_projects: snapshot
            .compose_projects
            .iter()
            .filter(|project| !compose_project_satisfied(project, running))
            .cloned()
            .collect(),
        standalone_containers: snapshot
            .standalone_containers
            .iter()
            .filter(|container| !container_running(container, running))
            .cloned()
            .collect(),
    }
}

fn compose_project_satisfied(project: &ComposeProject, running: &HashSet<String>) -> bool {
    !project.containers.is_empty()
        && project
            .containers
            .iter()
            .all(|container| container_running(container, running))
}

fn container_running(container: &ContainerResource, running: &HashSet<String>) -> bool {
    running.contains(&normalize_container_name(&container.name))
}

fn running_container_names(snapshot: &DockerSnapshot) -> HashSet<String> {
    snapshot
        .compose_projects
        .iter()
        .flat_map(|project| project.containers.iter())
        .chain(snapshot.standalone_containers.iter())
        .map(|container| normalize_container_name(&container.name))
        .collect()
}

fn normalize_container_name(name: &str) -> String {
    name.trim().trim_start_matches('/').to_ascii_lowercase()
}

fn apply_inner_report(
    report: &mut ConvergentDockerRestoreReport,
    inner: DockerRestoreReport,
) {
    report.resources_restored = inner.restored_resources;
    report.warnings.extend(inner.warnings);
    report.failures.extend(inner.failures);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(name: &str) -> ContainerResource {
        ContainerResource {
            id: name.to_owned(),
            name: name.to_owned(),
            image: Some("example:latest".to_owned()),
            ports: Vec::new(),
            mounts: Vec::new(),
            networks: Vec::new(),
        }
    }

    fn snapshot() -> DockerSnapshot {
        DockerSnapshot {
            status: DockerStatus::Available,
            context: Some("default".to_owned()),
            message: None,
            compose_projects: vec![ComposeProject {
                name: "app".to_owned(),
                working_directory: Some("/work".to_owned()),
                config_files: vec!["/work/compose.yml".to_owned()],
                services: vec!["web".to_owned(), "db".to_owned()],
                containers: vec![container("app-web-1"), container("app-db-1")],
            }],
            standalone_containers: vec![container("redis")],
        }
    }

    #[test]
    fn fully_running_resources_are_removed_from_restore_plan() {
        let saved = snapshot();
        let running = HashSet::from([
            "app-web-1".to_owned(),
            "app-db-1".to_owned(),
            "redis".to_owned(),
        ]);
        let missing = missing_snapshot(&saved, &running);
        assert!(missing.compose_projects.is_empty());
        assert!(missing.standalone_containers.is_empty());
    }

    #[test]
    fn partially_running_compose_project_is_restored_as_one_group() {
        let saved = snapshot();
        let running = HashSet::from(["app-web-1".to_owned(), "redis".to_owned()]);
        let missing = missing_snapshot(&saved, &running);
        assert_eq!(missing.compose_projects.len(), 1);
        assert!(missing.standalone_containers.is_empty());
    }

    #[test]
    fn container_name_matching_ignores_case_and_docker_leading_slash() {
        let running = HashSet::from(["redis".to_owned()]);
        assert!(container_running(&container("/REDIS"), &running));
    }
}
