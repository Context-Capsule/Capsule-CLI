use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";
const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";
const COMPOSE_WORKING_DIR_LABEL: &str = "com.docker.compose.project.working_dir";
const COMPOSE_CONFIG_FILES_LABEL: &str = "com.docker.compose.project.config_files";
const DOCKER_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);
const DOCKER_RESTORE_TIMEOUT: Duration = Duration::from_secs(90);
const DOCKER_POLL_INTERVAL: Duration = Duration::from_millis(50);
static CAPTURE_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DockerStatus {
    Available,
    Unavailable,
    NotRequested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerSnapshot {
    pub status: DockerStatus,
    pub context: Option<String>,
    pub message: Option<String>,
    pub compose_projects: Vec<ComposeProject>,
    pub standalone_containers: Vec<ContainerResource>,
}

impl DockerSnapshot {
    pub fn not_requested() -> Self {
        Self {
            status: DockerStatus::NotRequested,
            context: None,
            message: None,
            compose_projects: Vec::new(),
            standalone_containers: Vec::new(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: DockerStatus::Unavailable,
            context: None,
            message: Some(message.into()),
            compose_projects: Vec::new(),
            standalone_containers: Vec::new(),
        }
    }

    pub fn running_container_count(&self) -> usize {
        self.standalone_containers.len()
            + self
                .compose_projects
                .iter()
                .map(|project| project.containers.len())
                .sum::<usize>()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposeProject {
    pub name: String,
    pub working_directory: Option<String>,
    pub config_files: Vec<String>,
    pub services: Vec<String>,
    pub containers: Vec<ContainerResource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerResource {
    pub id: String,
    pub name: String,
    pub image: Option<String>,
    pub ports: Vec<PortBinding>,
    pub mounts: Vec<MountInfo>,
    pub networks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortBinding {
    pub container_port: String,
    pub host_ip: Option<String>,
    pub host_port: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountInfo {
    pub kind: Option<String>,
    pub source: Option<String>,
    pub destination: Option<String>,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerRestoreReport {
    pub attempted_resources: usize,
    pub restored_resources: usize,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

impl DockerRestoreReport {
    pub fn success(&self) -> bool {
        self.failures.is_empty()
    }
}

#[derive(Debug, Deserialize)]
struct RawContainer {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Config")]
    config: RawConfig,
    #[serde(rename = "State")]
    state: RawState,
    #[serde(rename = "Mounts", default)]
    mounts: Vec<RawMount>,
    #[serde(rename = "NetworkSettings", default)]
    network_settings: RawNetworkSettings,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(rename = "Image")]
    image: Option<String>,
    #[serde(rename = "Labels", default)]
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct RawState {
    #[serde(rename = "Running")]
    running: bool,
}

#[derive(Debug, Deserialize)]
struct RawMount {
    #[serde(rename = "Type")]
    kind: Option<String>,
    #[serde(rename = "Source")]
    source: Option<String>,
    #[serde(rename = "Destination")]
    destination: Option<String>,
    #[serde(rename = "RW", default)]
    writable: bool,
}

#[derive(Debug, Default, Deserialize)]
struct RawNetworkSettings {
    #[serde(rename = "Ports", default)]
    ports: BTreeMap<String, Option<Vec<RawPortBinding>>>,
    #[serde(rename = "Networks", default)]
    networks: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RawPortBinding {
    #[serde(rename = "HostIp")]
    host_ip: Option<String>,
    #[serde(rename = "HostPort")]
    host_port: Option<String>,
}

#[derive(Debug)]
struct ComposeAccumulator {
    name: String,
    working_directory: Option<String>,
    config_files: BTreeSet<String>,
    services: BTreeSet<String>,
    containers: Vec<ContainerResource>,
}

pub fn discover() -> DockerSnapshot {
    if let Err(error) = docker_server_available() {
        return DockerSnapshot::unavailable(error);
    }

    let context = docker_output(&["context", "show"]).ok();
    let ids = match docker_output(&["ps", "-q"]) {
        Ok(output) => output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>(),
        Err(error) => return DockerSnapshot::unavailable(error),
    };

    if ids.is_empty() {
        return DockerSnapshot {
            status: DockerStatus::Available,
            context,
            message: None,
            compose_projects: Vec::new(),
            standalone_containers: Vec::new(),
        };
    }

    let mut args = vec!["inspect".to_owned()];
    args.extend(ids);
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();

    let inspect_json = match docker_output(&arg_refs) {
        Ok(output) => output,
        Err(error) => {
            return DockerSnapshot {
                status: DockerStatus::Available,
                context,
                message: Some(format!(
                    "Docker is available, but container inspection failed: {error}"
                )),
                compose_projects: Vec::new(),
                standalone_containers: Vec::new(),
            };
        }
    };

    match parse_inspect_json(&inspect_json) {
        Ok((compose_projects, standalone_containers)) => DockerSnapshot {
            status: DockerStatus::Available,
            context,
            message: None,
            compose_projects,
            standalone_containers,
        },
        Err(error) => DockerSnapshot {
            status: DockerStatus::Available,
            context,
            message: Some(format!(
                "Docker inspection output could not be parsed: {error}"
            )),
            compose_projects: Vec::new(),
            standalone_containers: Vec::new(),
        },
    }
}

pub fn restore(snapshot: &DockerSnapshot) -> DockerRestoreReport {
    let mut report = DockerRestoreReport {
        attempted_resources: snapshot.compose_projects.len() + snapshot.standalone_containers.len(),
        restored_resources: 0,
        warnings: Vec::new(),
        failures: Vec::new(),
    };

    if !matches!(snapshot.status, DockerStatus::Available) {
        report
            .failures
            .push(snapshot.message.clone().unwrap_or_else(|| {
                "Docker was not available when this capsule was captured".to_owned()
            }));
        return report;
    }

    if let Err(error) = docker_server_available() {
        report.failures.push(error);
        return report;
    }

    for project in &snapshot.compose_projects {
        match restore_compose_project(project) {
            Ok(warning) => {
                report.restored_resources += 1;
                if let Some(warning) = warning {
                    report.warnings.push(warning);
                }
            }
            Err(error) => report.failures.push(error),
        }
    }

    for container in &snapshot.standalone_containers {
        match restore_existing_container(container) {
            Ok(()) => report.restored_resources += 1,
            Err(error) => report.failures.push(error),
        }
    }

    report
}

fn docker_server_available() -> Result<String, String> {
    docker_output(&["version", "--format", "{{.Server.Version}}"])
        .map_err(|error| format!("Docker is unavailable: {error}"))
}

fn docker_output(args: &[&str]) -> Result<String, String> {
    let output = run_docker_command(args, DOCKER_DISCOVERY_TIMEOUT, None)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        return Err(if detail.is_empty() {
            format!("'docker {}' exited with {}", args.join(" "), output.status)
        } else {
            detail
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn parse_inspect_json(
    json: &str,
) -> Result<(Vec<ComposeProject>, Vec<ContainerResource>), serde_json::Error> {
    let raw = serde_json::from_str::<Vec<RawContainer>>(json)?;
    Ok(group_containers(raw))
}

fn group_containers(
    raw_containers: Vec<RawContainer>,
) -> (Vec<ComposeProject>, Vec<ContainerResource>) {
    let mut projects = BTreeMap::<String, ComposeAccumulator>::new();
    let mut standalone = Vec::new();

    for raw in raw_containers
        .into_iter()
        .filter(|container| container.state.running)
    {
        let labels = raw.config.labels.clone().unwrap_or_default();
        let resource = to_resource(&raw);

        let Some(project_name) = labels
            .get(COMPOSE_PROJECT_LABEL)
            .filter(|value| !value.is_empty())
        else {
            standalone.push(resource);
            continue;
        };

        let project = projects
            .entry(project_name.clone())
            .or_insert_with(|| ComposeAccumulator {
                name: project_name.clone(),
                working_directory: labels.get(COMPOSE_WORKING_DIR_LABEL).cloned(),
                config_files: BTreeSet::new(),
                services: BTreeSet::new(),
                containers: Vec::new(),
            });

        if project.working_directory.is_none() {
            project.working_directory = labels.get(COMPOSE_WORKING_DIR_LABEL).cloned();
        }

        if let Some(files) = labels.get(COMPOSE_CONFIG_FILES_LABEL) {
            for file in split_compose_files(files) {
                project.config_files.insert(file);
            }
        }

        if let Some(service) = labels
            .get(COMPOSE_SERVICE_LABEL)
            .filter(|value| !value.is_empty())
        {
            project.services.insert(service.clone());
        }

        project.containers.push(resource);
    }

    standalone.sort_by(|left, right| left.name.cmp(&right.name));
    let mut compose_projects = projects
        .into_values()
        .map(|project| ComposeProject {
            name: project.name,
            working_directory: project.working_directory,
            config_files: project.config_files.into_iter().collect(),
            services: project.services.into_iter().collect(),
            containers: project.containers,
        })
        .collect::<Vec<_>>();
    compose_projects.sort_by(|left, right| left.name.cmp(&right.name));

    (compose_projects, standalone)
}

fn to_resource(raw: &RawContainer) -> ContainerResource {
    let mut ports = Vec::new();
    for (container_port, bindings) in &raw.network_settings.ports {
        match bindings {
            Some(bindings) if !bindings.is_empty() => {
                for binding in bindings {
                    ports.push(PortBinding {
                        container_port: container_port.clone(),
                        host_ip: binding.host_ip.clone(),
                        host_port: binding.host_port.clone(),
                    });
                }
            }
            _ => ports.push(PortBinding {
                container_port: container_port.clone(),
                host_ip: None,
                host_port: None,
            }),
        }
    }

    let mut networks = raw
        .network_settings
        .networks
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    networks.sort();

    ContainerResource {
        id: raw.id.clone(),
        name: raw.name.trim_start_matches('/').to_owned(),
        image: raw.config.image.clone(),
        ports,
        mounts: raw
            .mounts
            .iter()
            .map(|mount| MountInfo {
                kind: mount.kind.clone(),
                source: mount.source.clone(),
                destination: mount.destination.clone(),
                read_only: !mount.writable,
            })
            .collect(),
        networks,
    }
}

fn split_compose_files(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn restore_compose_project(project: &ComposeProject) -> Result<Option<String>, String> {
    let usable_config = !project.config_files.is_empty()
        && project
            .config_files
            .iter()
            .all(|path| Path::new(path).is_file());
    let usable_working_directory = project
        .working_directory
        .as_deref()
        .is_some_and(|path| Path::new(path).is_dir());

    if usable_config || usable_working_directory {
        let args = build_compose_args(project, usable_config);
        let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let working_directory = if usable_working_directory {
            project.working_directory.as_deref().map(Path::new)
        } else {
            None
        };
        let output = run_docker_command(&arg_refs, DOCKER_RESTORE_TIMEOUT, working_directory)
            .map_err(|error| {
                format!(
                    "Compose project '{}': failed to start Docker Compose: {error}",
                    project.name
                )
            })?;

        if output.status.success() {
            return Ok(None);
        }

        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let fallback = restore_project_containers_individually(project);
        return match fallback {
            Ok(()) => Ok(Some(format!(
                "Compose project '{}' could not be recreated with Docker Compose{}; existing containers were started directly instead",
                project.name,
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            ))),
            Err(fallback_error) => Err(format!(
                "Compose project '{}': compose restore failed{}; fallback also failed: {fallback_error}",
                project.name,
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            )),
        };
    }

    restore_project_containers_individually(project).map(|()| {
        Some(format!(
            "Compose project '{}' configuration files/working directory were not available; existing containers were started directly",
            project.name
        ))
    })
}

fn build_compose_args(project: &ComposeProject, include_config_files: bool) -> Vec<String> {
    let mut args = vec!["compose".to_owned()];

    if include_config_files {
        for file in &project.config_files {
            args.push("-f".to_owned());
            args.push(file.clone());
        }
    }

    args.push("--project-name".to_owned());
    args.push(project.name.clone());
    args.push("up".to_owned());
    args.push("-d".to_owned());
    args.extend(project.services.iter().cloned());
    args
}

fn restore_project_containers_individually(project: &ComposeProject) -> Result<(), String> {
    let mut failures = Vec::new();
    for container in &project.containers {
        if let Err(error) = restore_existing_container(container) {
            failures.push(error);
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn restore_existing_container(container: &ContainerResource) -> Result<(), String> {
    let inspect_args = [
        "inspect",
        "--type",
        "container",
        "--format",
        "{{.State.Running}}",
        container.name.as_str(),
    ];
    let inspect =
        run_docker_command(&inspect_args, DOCKER_DISCOVERY_TIMEOUT, None).map_err(|error| {
            format!(
                "Container '{}': failed to query Docker: {error}",
                container.name
            )
        })?;

    if !inspect.status.success() {
        return Err(format!(
            "Container '{}' no longer exists. Context Capsule will not recreate a standalone container because environment variables and other secret-bearing runtime configuration are intentionally not captured",
            container.name
        ));
    }

    if String::from_utf8_lossy(&inspect.stdout).trim() == "true" {
        return Ok(());
    }

    let start_args = ["start", container.name.as_str()];
    let output = run_docker_command(&start_args, DOCKER_RESTORE_TIMEOUT, None)
        .map_err(|error| format!("Container '{}': failed to start: {error}", container.name))?;

    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(format!(
            "Container '{}': docker start failed{}",
            container.name,
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ))
    }
}

fn run_docker_command(
    args: &[&str],
    timeout: Duration,
    working_directory: Option<&Path>,
) -> Result<Output, String> {
    let description = format!("docker {}", args.join(" "));
    let capture = CaptureFiles::create()
        .ok_or_else(|| format!("failed to create output capture files for '{description}'"))?;
    let mut command = Command::new("docker");
    command.args(args);
    if let Some(directory) = working_directory {
        command.current_dir(directory);
    }
    command
        .stdout(Stdio::from(
            capture
                .stdout_writer
                .as_ref()
                .and_then(|file| file.try_clone().ok())
                .ok_or_else(|| format!("failed to capture stdout for '{description}'"))?,
        ))
        .stderr(Stdio::from(
            capture
                .stderr_writer
                .as_ref()
                .and_then(|file| file.try_clone().ok())
                .ok_or_else(|| format!("failed to capture stderr for '{description}'"))?,
        ));

    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to run '{description}': {error}"))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => thread::sleep(DOCKER_POLL_INTERVAL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "'{description}' timed out after {} second(s)",
                    timeout.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("failed while waiting for '{description}': {error}"));
            }
        }
    };

    capture
        .finish(status)
        .ok_or_else(|| format!("failed to read output from '{description}'"))
}

struct CaptureFiles {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    stdout_writer: Option<File>,
    stderr_writer: Option<File>,
}

impl CaptureFiles {
    fn create() -> Option<Self> {
        let id = CAPTURE_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos();
        let prefix = format!(
            "context-capsule-docker-{}-{timestamp}-{id}",
            std::process::id()
        );
        let directory = std::env::temp_dir();
        let stdout_path = directory.join(format!("{prefix}.stdout"));
        let stderr_path = directory.join(format!("{prefix}.stderr"));
        let stdout_writer = File::create(&stdout_path).ok()?;
        let stderr_writer = match File::create(&stderr_path) {
            Ok(file) => file,
            Err(_) => {
                let _ = fs::remove_file(&stdout_path);
                return None;
            }
        };

        Some(Self {
            stdout_path,
            stderr_path,
            stdout_writer: Some(stdout_writer),
            stderr_writer: Some(stderr_writer),
        })
    }

    fn close_writers(&mut self) {
        drop(self.stdout_writer.take());
        drop(self.stderr_writer.take());
    }

    fn finish(mut self, status: ExitStatus) -> Option<Output> {
        self.close_writers();
        let stdout = fs::read(&self.stdout_path).unwrap_or_default();
        let stderr = fs::read(&self.stderr_path).unwrap_or_default();
        let _ = fs::remove_file(&self.stdout_path);
        let _ = fs::remove_file(&self.stderr_path);
        Some(Output {
            status,
            stdout,
            stderr,
        })
    }
}

impl Drop for CaptureFiles {
    fn drop(&mut self) {
        self.close_writers();
        let _ = fs::remove_file(&self.stdout_path);
        let _ = fs::remove_file(&self.stderr_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
[
  {
    "Id": "compose-1",
    "Name": "/capsule-web-1",
    "Config": {
      "Image": "example/web:1",
      "Env": ["TOKEN=do-not-store-this"],
      "Labels": {
        "com.docker.compose.project": "capsule-ci",
        "com.docker.compose.service": "web",
        "com.docker.compose.project.working_dir": "C:\\work\\capsule",
        "com.docker.compose.project.config_files": "C:\\work\\capsule\\compose.yml"
      }
    },
    "State": { "Running": true },
    "Mounts": [
      { "Type": "bind", "Source": "C:\\work\\capsule", "Destination": "/app", "RW": true }
    ],
    "NetworkSettings": {
      "Ports": { "3000/tcp": [{ "HostIp": "127.0.0.1", "HostPort": "3000" }] },
      "Networks": { "capsule-ci_default": {} }
    }
  },
  {
    "Id": "standalone-1",
    "Name": "/redis-dev",
    "Config": {
      "Image": "redis:7",
      "Env": ["PASSWORD=also-do-not-store"],
      "Labels": {}
    },
    "State": { "Running": true },
    "Mounts": [],
    "NetworkSettings": {
      "Ports": { "6379/tcp": null },
      "Networks": { "bridge": {} }
    }
  },
  {
    "Id": "stopped-1",
    "Name": "/stopped",
    "Config": { "Image": "alpine", "Labels": {} },
    "State": { "Running": false },
    "Mounts": [],
    "NetworkSettings": { "Ports": {}, "Networks": {} }
  }
]
"#;

    #[test]
    fn groups_running_compose_and_standalone_containers() {
        let (projects, standalone) = parse_inspect_json(FIXTURE).expect("fixture parses");

        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "capsule-ci");
        assert_eq!(projects[0].services, vec!["web"]);
        assert_eq!(projects[0].containers.len(), 1);
        assert_eq!(projects[0].containers[0].name, "capsule-web-1");
        assert_eq!(standalone.len(), 1);
        assert_eq!(standalone[0].name, "redis-dev");
        assert_eq!(standalone[0].ports[0].container_port, "6379/tcp");
    }

    #[test]
    fn captured_snapshot_does_not_serialize_environment_secrets() {
        let (projects, standalone) = parse_inspect_json(FIXTURE).expect("fixture parses");
        let snapshot = DockerSnapshot {
            status: DockerStatus::Available,
            context: Some("desktop-linux".to_owned()),
            message: None,
            compose_projects: projects,
            standalone_containers: standalone,
        };

        let json = serde_json::to_string(&snapshot).expect("snapshot serializes");
        assert!(!json.contains("do-not-store-this"));
        assert!(!json.contains("also-do-not-store"));
        assert!(!json.contains("TOKEN"));
        assert!(!json.contains("PASSWORD"));
    }

    #[test]
    fn compose_restore_command_uses_project_files_and_services() {
        let project = ComposeProject {
            name: "capsule-ci".to_owned(),
            working_directory: Some("/work/project".to_owned()),
            config_files: vec!["/work/project/compose.yml".to_owned()],
            services: vec!["api".to_owned(), "db".to_owned()],
            containers: Vec::new(),
        };

        assert_eq!(
            build_compose_args(&project, true),
            vec![
                "compose",
                "-f",
                "/work/project/compose.yml",
                "--project-name",
                "capsule-ci",
                "up",
                "-d",
                "api",
                "db",
            ]
        );
    }

    #[test]
    fn compose_file_label_is_split_and_empty_entries_are_removed() {
        assert_eq!(
            split_compose_files("one.yml, two.yml,,"),
            vec!["one.yml", "two.yml"]
        );
    }
}
