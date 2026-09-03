//! Concrete composition root for the build/verify/clean application use cases.

use crate::artifacts::{OWNER_MARKER, cleanup_owned_temp_source};
use crate::buildkit::{
    BuildKit, BuildOptions, BuildRequest, CacheConfig, ImageOutput, NeverCancelled,
    ProcessExecutor, ProcessInvocation, Progress, SystemProcessExecutor,
};
use crate::docker_runner::{DockerRunner, SystemClock, SystemDockerExecutor};
use crate::doctor::{DoctorProbe, SystemDoctorProbe};
use crate::registry::{DockerRegistry, OciRegistry, SystemRegistryExecutor};
use crate::snapshot::GitSnapshotter;
use crate::task_image::{TaskImageBuilder, TaskImageOptions, TaskImageRequest};
use repo_sandbox_core::AppError;
use repo_sandbox_core::application::{
    CleanCandidate, CleanPlan, CleanPort, CleanRequest, CleanResult, ExecutionPlan, ResourceKind,
    WorkflowMode, WorkflowPort, WorkflowResult,
};
use repo_sandbox_core::build::ImageRef;
use repo_sandbox_core::registry::{PublishRequest, RegistryRepository, RegistryTag};
use repo_sandbox_core::runner::{
    ConfigSummary, RunResources, RunSpec, RunStatus, SecretMount, StepPhase, write_report_json,
};
use repo_sandbox_core::snapshot::{
    CleanupPolicy, ExternalSecret, GitAuthentication, SnapshotOptions, SourceSpec,
};
use repo_sandbox_core::task_image::ConfigurationDigest;
use serde_json;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemWorkflow;

impl WorkflowPort for SystemWorkflow {
    fn execute(
        &self,
        mode: WorkflowMode,
        plan: &ExecutionPlan,
    ) -> Result<WorkflowResult, AppError> {
        let repository = repository_path(plan)?;
        let state = repository.join(".repo-sandbox");
        let task_id = task_id();
        let repository_id = repository_id(&repository)?;
        let journal = ManifestJournal::create(&state, &task_id)?;
        let report_path = plan
            .request
            .report
            .clone()
            .unwrap_or_else(|| state.join("reports").join(format!("{task_id}.json")));
        let _report_reservation = ReportReservation::create(&report_path)?;
        let registry = plan.template.execution.registry.as_ref();
        if plan.request.push && registry.is_none() {
            return Err(AppError::Configuration(
                "--push requires execution.registry.repository in the central profile".into(),
            ));
        }
        preflight(plan, &repository)?;
        let cache = state.join("cache");
        fs::create_dir_all(&cache).map_err(environment("create owned cache"))?;
        fs::write(cache.join(OWNER_MARKER), &repository_id)
            .map_err(environment("mark owned cache"))?;
        journal.append(&[CleanCandidate {
            task_id: task_id.clone(),
            repository_id: repository_id.clone(),
            kind: ResourceKind::Cache,
            identifier: cache.display().to_string(),
            owner: repository_id.clone(),
        }])?;

        let source = match (&plan.request.repository, &plan.request.git_ref) {
            (Some(value), reference) if is_remote(value) => SourceSpec::RemoteGit {
                repository: value.clone(),
                git_ref: reference.clone().unwrap_or_else(|| "HEAD".into()),
            },
            _ => SourceSpec::LocalDirectory(repository.clone()),
        };
        let mut materialized = GitSnapshotter::default()
            .with_authentication(environment_git_authentication(source_repository(&source)))
            .create(
                &source,
                SnapshotOptions {
                    recurse_submodules: plan.request.recurse_submodules,
                    cleanup: CleanupPolicy::Delete,
                },
            )
            .map_err(|error| AppError::Environment(error.to_string()))?;
        let catalog_root = catalog_root(&repository)?;
        let cache_import = cache.join("environment");
        let cache_export = cache.join("environment-next");
        if cache_export.exists() {
            fs::remove_dir_all(&cache_export)
                .map_err(environment("remove stale owned cache export"))?;
        }
        let cache_options = CacheConfig {
            imports: cache_import
                .join("index.json")
                .exists()
                .then(|| format!("type=local,src={}", docker_host_path(&cache_import)))
                .into_iter()
                .collect(),
            exports: vec![format!(
                "type=local,dest={},mode=max",
                docker_host_path(&cache_export)
            )],
        };
        let environment_ref =
            ImageRef::new(format!("repo-sandbox-env:{}", short_digest(&plan.digest)))
                .map_err(AppError::Configuration)?;
        let environment_image = BuildKit::new(SystemProcessExecutor)
            .build(
                BuildRequest::environment(
                    &plan.template,
                    &catalog_root,
                    environment_ref,
                    BuildOptions {
                        progress: Progress::Plain,
                        cache: cache_options,
                        ..BuildOptions::default()
                    },
                ),
                &NeverCancelled,
            )
            .map_err(|error| AppError::Environment(error.to_string()))?;
        if cache_export.exists() {
            if cache_import.exists() {
                let _ = fs::remove_dir_all(&cache_import);
            }
            let _ = fs::rename(&cache_export, &cache_import);
        }

        let configuration_digest =
            ConfigurationDigest::parse(&plan.digest).map_err(AppError::Configuration)?;
        let image_repository = "repo-sandbox-task";
        let task_image = TaskImageBuilder::new(SystemProcessExecutor)
            .build(
                TaskImageRequest {
                    environment: &environment_image,
                    materialized: &materialized,
                    template_id: &plan.template.template_id,
                    template_version: &plan.template.template_version,
                    platform: plan.request.platform,
                    configuration_digest: &configuration_digest,
                    created: "1970-01-01T00:00:00Z",
                    repository: image_repository,
                    options: TaskImageOptions {
                        progress: Progress::Plain,
                        output: ImageOutput::Load,
                        ..TaskImageOptions::default()
                    },
                },
                &NeverCancelled,
            )
            .map_err(|error| AppError::Environment(error.to_string()))?;
        journal.append(&[CleanCandidate {
            task_id: task_id.clone(),
            repository_id: repository_id.clone(),
            kind: ResourceKind::Image,
            identifier: task_image.image.digest.to_string(),
            owner: task_image.identity.oci_value(),
        }])?;

        let execution = &plan.template.execution;
        let secret_root = tempfile::Builder::new()
            .prefix("repo-sandbox-secrets-")
            .tempdir()
            .map_err(environment("create runtime secret directory"))?;
        let mut secret_mounts = Vec::new();
        for name in &execution.secret_environment {
            let value = std::env::var_os(name).ok_or_else(|| {
                AppError::Configuration(format!("required secret environment `{name}` is not set"))
            })?;
            let path = secret_root.path().join(name);
            fs::write(&path, value.to_string_lossy().as_bytes())
                .map_err(environment("materialize runtime secret"))?;
            secret_mounts.push(SecretMount {
                environment: name.clone(),
                source: path,
            });
        }
        let build = execution
            .build
            .iter()
            .map(|step| repo_sandbox_core::runner::RunStep {
                name: step.name.clone(),
                command: step.command.clone(),
            })
            .collect();
        let test = if mode == WorkflowMode::Verify {
            execution
                .test
                .iter()
                .map(|step| repo_sandbox_core::runner::RunStep {
                    name: step.name.clone(),
                    command: step.command.clone(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let artifact_root = state.join("artifacts").join(&task_id);
        let spec = RunSpec {
            task_id: task_id.clone(),
            image: task_image.image.image.clone(),
            image_digest: task_image.image.digest.clone(),
            source_snapshot: materialized.snapshot.clone(),
            config_summary: ConfigSummary {
                template_id: plan.template.template_id.clone(),
                plan_digest: plan.digest.clone(),
                platform: plan.request.platform,
                build_steps: execution
                    .build
                    .iter()
                    .map(|step| step.name.clone())
                    .collect(),
                test_steps: if mode == WorkflowMode::Verify {
                    execution
                        .test
                        .iter()
                        .map(|step| step.name.clone())
                        .collect()
                } else {
                    Vec::new()
                },
                artifact_directories: execution.artifact_directories.clone(),
            },
            platform: plan.request.platform,
            build,
            test,
            resources: RunResources {
                cpu_count: execution.resources.cpu,
                memory_mb: execution.resources.memory_mb,
                temporary_storage_mb: execution.resources.temporary_storage_mb,
            },
            timeout_ms: u64::from(execution.timeout_seconds) * 1000,
            fail_fast: execution.fail_fast,
            environment_names: execution.environment_allow.clone(),
            secret_mounts,
            artifact_export_root: (!execution.artifact_directories.is_empty())
                .then_some(artifact_root),
            // A retained bind mount could keep the deleted secret inode readable.
            // Security therefore wins over diagnostics for secret-bearing jobs.
            keep_on_failure: plan.request.keep_on_failure
                && secret_root
                    .path()
                    .read_dir()
                    .map(|mut entries| entries.next().is_none())
                    .unwrap_or(false),
        };
        let mut report = DockerRunner::new(SystemDockerExecutor, SystemClock::default())
            .run_with_container_hook(&spec, |container| {
                journal
                    .append(&[CleanCandidate {
                        task_id: task_id.clone(),
                        repository_id: repository_id.clone(),
                        kind: ResourceKind::Container,
                        identifier: container.to_owned(),
                        owner: task_id.clone(),
                    }])
                    .map_err(|error| error.to_string())
            })
            .map_err(|error| AppError::Configuration(error.to_string()))?;

        let failed = report.status != RunStatus::Succeeded;
        if report.cleanup == repo_sandbox_core::runner::CleanupResult::RetainedOnFailure {
            materialized.retain_on_failure();
            if materialized.is_automatically_cleaned() == false
                && is_remote(plan.request.repository.as_deref().unwrap_or(""))
            {
                fs::write(materialized.path().join(OWNER_MARKER), &task_id)
                    .map_err(environment("mark retained source"))?;
                journal.append(&[CleanCandidate {
                    task_id: task_id.clone(),
                    repository_id: repository_id.clone(),
                    kind: ResourceKind::Source,
                    identifier: materialized.path().display().to_string(),
                    owner: task_id.clone(),
                }])?;
            }
        }

        let published = if plan.request.push && !failed {
            let policy = registry.expect("checked registry");
            let repository =
                RegistryRepository::new(&policy.repository).map_err(AppError::Configuration)?;
            let aliases = policy
                .aliases
                .iter()
                .map(|value| RegistryTag::new(value).map_err(AppError::Configuration))
                .collect::<Result<Vec<_>, _>>()?;
            let seeded = seed_registry(
                &task_image.image.image,
                &repository,
                task_image.identity.as_str(),
            )?;
            Some(
                DockerRegistry::new(SystemRegistryExecutor)
                    .publish(
                        &PublishRequest {
                            source: seeded,
                            repository,
                            digest: task_image.image.digest.clone(),
                            platform_digests: task_image.image.platform_digests.clone(),
                            aliases,
                        },
                        &NeverCancelled,
                    )
                    .map_err(|error| AppError::Environment(error.to_string()))?,
            )
        } else {
            None
        };
        report.published = published.clone();

        write_report_json(&report, &report_path).map_err(environment("write atomic report"))?;
        let result = WorkflowResult {
            plan_digest: plan.digest.clone(),
            report: report.clone(),
            published,
        };
        match &report.status {
            RunStatus::Succeeded
                if report.cleanup != repo_sandbox_core::runner::CleanupResult::Failed =>
            {
                Ok(result)
            }
            RunStatus::CommandFailed {
                phase: StepPhase::Build,
                ..
            } => Err(AppError::BuildFailed(format!(
                "task {task_id}; report {}",
                report_path.display()
            ))),
            RunStatus::CommandFailed {
                phase: StepPhase::Test,
                ..
            } => Err(AppError::TestFailed(format!(
                "task {task_id}; report {}",
                report_path.display()
            ))),
            RunStatus::TimedOut
            | RunStatus::ResourceExceeded { .. }
            | RunStatus::InfrastructureFailed { .. } => Err(AppError::Environment(format!(
                "task {task_id} failed; report {}",
                report_path.display()
            ))),
            RunStatus::Succeeded => Err(AppError::Environment(format!(
                "task cleanup failed; report {}",
                report_path.display()
            ))),
        }
    }
}

impl CleanPort for SystemWorkflow {
    fn plan(&self, request: &CleanRequest) -> Result<CleanPlan, AppError> {
        let repository = request
            .repository
            .canonicalize()
            .map_err(environment("resolve repository"))?;
        let expected_repository = repository_id(&repository)?;
        let manifests = repository.join(".repo-sandbox").join("tasks");
        let mut plan = CleanPlan::default();
        if !manifests.exists() {
            return Ok(plan);
        }
        for entry in fs::read_dir(manifests).map_err(environment("read task manifests"))? {
            let path = entry
                .map_err(environment("read task manifest entry"))?
                .path();
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            let candidates: Vec<CleanCandidate> = serde_json::from_slice(
                &fs::read(&path).map_err(environment("read task manifest"))?,
            )
            .map_err(|error| AppError::Environment(format!("parse {}: {error}", path.display())))?;
            for candidate in candidates {
                if candidate.repository_id != expected_repository {
                    plan.refused.push(format!(
                        "{}: repository owner mismatch",
                        candidate.identifier
                    ));
                    continue;
                }
                if candidate.kind == ResourceKind::Cache
                    && PathBuf::from(&candidate.identifier)
                        != repository.join(".repo-sandbox").join("cache")
                {
                    plan.refused
                        .push(format!("{}: cache boundary mismatch", candidate.identifier));
                    continue;
                }
                if candidate.kind == ResourceKind::Image && !request.include_images {
                    plan.refused
                        .push(format!("{}: images not requested", candidate.identifier));
                    continue;
                }
                if candidate.kind == ResourceKind::Cache && !request.include_cache {
                    plan.refused
                        .push(format!("{}: cache not requested", candidate.identifier));
                    continue;
                }
                plan.candidates.push(candidate);
            }
        }
        Ok(plan)
    }

    fn execute(&self, plan: &CleanPlan, dry_run: bool) -> Result<CleanResult, AppError> {
        let mut result = CleanResult {
            dry_run,
            skipped: plan.refused.clone(),
            ..CleanResult::default()
        };
        for candidate in &plan.candidates {
            if dry_run {
                result.skipped.push(format!(
                    "dry-run: {:?} {}",
                    candidate.kind, candidate.identifier
                ));
                continue;
            }
            match remove_candidate(candidate) {
                Ok(true) => result.succeeded.push(candidate.clone()),
                Ok(false) => result.skipped.push(format!(
                    "{}: absent or still referenced",
                    candidate.identifier
                )),
                Err(error) => result
                    .failed
                    .push(format!("{}: {error}", candidate.identifier)),
            }
        }
        Ok(result)
    }
}

fn preflight(plan: &ExecutionPlan, repository: &Path) -> Result<(), AppError> {
    for args in [["info"].as_slice(), ["buildx", "inspect"].as_slice()] {
        let invocation = ProcessInvocation {
            program: "docker".into(),
            args: args.iter().map(|v| (*v).into()).collect(),
            current_dir: None,
        };
        let output = SystemProcessExecutor
            .execute(&invocation, &NeverCancelled)
            .map_err(environment("Docker preflight"))?;
        if output.exit_code != Some(0) {
            return Err(AppError::Environment(format!(
                "Docker preflight failed: {}",
                output.stderr.trim()
            )));
        }
    }
    let free = SystemDoctorProbe
        .available_space(repository)
        .map_err(environment("disk preflight"))?;
    if free < 1024 * 1024 * 1024 {
        return Err(AppError::Environment(format!(
            "disk preflight requires 1 GiB free, found {free} bytes"
        )));
    }
    if plan.request.push {
        let policy = plan
            .template
            .execution
            .registry
            .as_ref()
            .expect("validated before preflight");
        let registry = policy.repository.split('/').next().unwrap_or("");
        let (host, port) = registry
            .rsplit_once(':')
            .and_then(|(host, port)| port.parse::<u16>().ok().map(|port| (host, port)))
            .unwrap_or((registry, 443));
        SystemDoctorProbe
            .connect_registry(host, port, std::time::Duration::from_secs(3))
            .map_err(environment("registry preflight"))?;
    }
    Ok(())
}

fn repository_path(plan: &ExecutionPlan) -> Result<PathBuf, AppError> {
    if plan.request.repository.as_deref().is_some_and(is_remote) {
        return std::env::current_dir().map_err(environment(
            "resolve current repository for remote configuration",
        ));
    }
    let path = plan
        .request
        .repository
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir().map_err(environment("resolve current repository"))?);
    path.canonicalize()
        .map_err(environment("resolve repository"))
}

fn catalog_root(repository: &Path) -> Result<PathBuf, AppError> {
    let compiled = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    repository
        .ancestors()
        .map(Path::to_path_buf)
        .chain(compiled)
        .find(|root| {
            root.join("templates/rust-bazel/context/Dockerfile")
                .is_file()
        })
        .ok_or_else(|| {
            AppError::Environment("cannot locate bundled central template contexts".into())
        })
}

fn is_remote(value: &str) -> bool {
    value.contains("://") || value.starts_with("git@")
}
fn source_repository(source: &SourceSpec) -> &str {
    match source {
        SourceSpec::RemoteGit { repository, .. } => repository,
        SourceSpec::LocalDirectory(_) => "",
    }
}

/// Map opt-in fixture credentials to external references. Values are never
/// copied into configuration, argv, logs, or reports.
pub fn environment_git_authentication(repository: &str) -> GitAuthentication {
    if repository.starts_with("git@") || repository.starts_with("ssh://") {
        let known_hosts = std::env::var_os("REPO_SANDBOX_E2E_SSH_KNOWN_HOSTS").map(PathBuf::from);
        return std::env::var_os("REPO_SANDBOX_E2E_SSH_KEY")
            .map(PathBuf::from)
            .map(|private_key| GitAuthentication::SshKey {
                private_key,
                known_hosts: known_hosts.clone(),
            })
            .unwrap_or(GitAuthentication::SshAgent { known_hosts });
    }
    if repository.starts_with("https://") {
        if std::env::var_os("REPO_SANDBOX_E2E_HTTPS_TOKEN").is_some() {
            return GitAuthentication::HttpsToken {
                username: std::env::var("REPO_SANDBOX_E2E_HTTPS_USER")
                    .unwrap_or_else(|_| "git".into()),
                token: ExternalSecret::Environment("REPO_SANDBOX_E2E_HTTPS_TOKEN".into()),
            };
        }
        return GitAuthentication::HttpsCredentialHelper;
    }
    GitAuthentication::None
}
fn short_digest(value: &str) -> &str {
    value
        .strip_prefix("sha256:")
        .unwrap_or(value)
        .get(..24)
        .unwrap_or(value)
}
fn docker_host_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
}
fn task_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    )
}
fn repository_id(repository: &Path) -> Result<String, AppError> {
    let mut h = Sha256::new();
    h.update(repository.to_string_lossy().as_bytes());
    Ok(format!("sha256:{:x}", h.finalize()))
}
fn environment(operation: &'static str) -> impl FnOnce(std::io::Error) -> AppError {
    move |error| AppError::Environment(format!("{operation}: {error}"))
}

struct ReportReservation {
    path: PathBuf,
}
impl ReportReservation {
    fn create(report: &Path) -> Result<Self, AppError> {
        let parent = report.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(environment("create report directory"))?;
        if report.exists() {
            return Err(AppError::Configuration(format!(
                "report already exists: {}",
                report.display()
            )));
        }
        let name = report
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("report.json");
        let path = parent.join(format!(".{name}.repo-sandbox-reservation"));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                AppError::Configuration(format!(
                    "cannot reserve report {}: {error}",
                    report.display()
                ))
            })?;
        Ok(Self { path })
    }
}
impl Drop for ReportReservation {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct ManifestJournal {
    root: PathBuf,
    task_id: String,
    sequence: AtomicU64,
}

impl ManifestJournal {
    fn create(state: &Path, task_id: &str) -> Result<Self, AppError> {
        let root = state.join("tasks");
        fs::create_dir_all(&root).map_err(environment("create task manifest directory"))?;
        let journal = Self {
            root,
            task_id: task_id.into(),
            sequence: AtomicU64::new(0),
        };
        journal.append(&[])?;
        Ok(journal)
    }

    fn append(&self, candidates: &[CleanCandidate]) -> Result<(), AppError> {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        let final_path = self
            .root
            .join(format!("{}-{sequence:06}.json", self.task_id));
        let temporary = self.root.join(format!(
            ".{}-{sequence:06}.{}.tmp",
            self.task_id,
            std::process::id()
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(environment("create task manifest event"))?;
        file.write_all(
            &serde_json::to_vec_pretty(candidates)
                .map_err(|error| AppError::Environment(error.to_string()))?,
        )
        .map_err(environment("write task manifest event"))?;
        file.sync_all()
            .map_err(environment("sync task manifest event"))?;
        fs::rename(&temporary, &final_path).map_err(environment("publish task manifest event"))
    }
}

fn docker_output(args: &[&str]) -> Result<crate::buildkit::ProcessOutput, String> {
    let invocation = ProcessInvocation {
        program: "docker".into(),
        args: args.iter().map(|v| (*v).into()).collect(),
        current_dir: None,
    };
    SystemProcessExecutor
        .execute(&invocation, &NeverCancelled)
        .map_err(|e| e.to_string())
}

fn seed_registry(
    source: &ImageRef,
    repository: &RegistryRepository,
    identity: &str,
) -> Result<ImageRef, AppError> {
    let staging = repository.tagged(
        &RegistryTag::new(format!("staging-{}", &identity[..24]))
            .map_err(AppError::Configuration)?,
    );
    for args in [
        vec!["image", "tag", source.as_str(), staging.as_str()],
        vec!["push", staging.as_str()],
    ] {
        let output = docker_output(&args).map_err(AppError::Environment)?;
        if output.exit_code != Some(0) {
            return Err(AppError::Environment(format!(
                "registry seed push failed: {}",
                output.stderr.trim()
            )));
        }
    }
    Ok(staging)
}

fn remove_candidate(candidate: &CleanCandidate) -> Result<bool, String> {
    match candidate.kind {
        ResourceKind::Container => {
            let inspected = docker_output(&[
                "container",
                "inspect",
                "--format",
                "{{ index .Config.Labels \"io.repo-sandbox.task-id\" }}",
                &candidate.identifier,
            ])?;
            if inspected.exit_code != Some(0) {
                return Ok(false);
            }
            if inspected.stdout.trim() != candidate.owner {
                return Err("owner label mismatch".into());
            }
            let removed = docker_output(&["container", "rm", "--force", &candidate.identifier])?;
            if removed.exit_code == Some(0) {
                Ok(true)
            } else {
                Err(removed.stderr)
            }
        }
        ResourceKind::Image => {
            let inspected = docker_output(&[
                "image",
                "inspect",
                "--format",
                "{{ index .Config.Labels \"io.repo-sandbox.task.identity\" }}",
                &candidate.identifier,
            ])?;
            if inspected.exit_code != Some(0) {
                return Ok(false);
            }
            if inspected.stdout.trim() != candidate.owner {
                return Err("image owner label mismatch".into());
            }
            let references = docker_output(&[
                "container",
                "ls",
                "--all",
                "--quiet",
                "--filter",
                &format!("ancestor={}", candidate.identifier),
            ])?;
            if !references.stdout.trim().is_empty() {
                return Ok(false);
            }
            let removed = docker_output(&["image", "rm", &candidate.identifier])?;
            if removed.exit_code == Some(0) {
                Ok(true)
            } else {
                Err(removed.stderr)
            }
        }
        ResourceKind::Source => {
            let path = PathBuf::from(&candidate.identifier);
            let parent = path.parent().ok_or("source has no parent")?;
            cleanup_owned_temp_source(parent, &path, &candidate.owner)
                .map(|_| true)
                .map_err(|e| e.to_string())
        }
        ResourceKind::Cache => {
            let path = PathBuf::from(&candidate.identifier);
            if !path.exists() {
                return Ok(false);
            }
            let owner = fs::read_to_string(path.join(OWNER_MARKER)).map_err(|e| e.to_string())?;
            if owner != candidate.owner {
                return Err("cache owner marker mismatch".into());
            }
            fs::remove_dir_all(path)
                .map(|_| true)
                .map_err(|e| e.to_string())
        }
        ResourceKind::Builder => {
            Err("builder cleanup requires an exact adapter ownership record".into())
        }
    }
}
