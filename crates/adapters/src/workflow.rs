//! Concrete composition root for the build/verify/clean application use cases.

use crate::artifacts::{OWNER_MARKER, cleanup_owned_temp_source};
use crate::buildkit::{
    BuildKit, BuildOptions, BuildRequest, CacheConfig, ImageOutput, NeverCancelled,
    ProcessExecutor, ProcessInvocation, Progress, SystemProcessExecutor,
};
use crate::cancellation::ProcessCancellation;
use crate::docker_runner::{DockerRunner, SystemClock, SystemDockerExecutor};
use crate::doctor::{DoctorProbe, SystemDoctorProbe};
use crate::registry::{DockerRegistry, OciRegistry, SystemRegistryExecutor};
use crate::snapshot::GitSnapshotter;
use crate::task_image::{TaskImageBuilder, TaskImageOptions, TaskImageRequest};
use repo_sandbox_core::AppError;
use repo_sandbox_core::application::{
    CleanCandidate, CleanPlan, CleanPort, CleanRequest, CleanResult, ExecutionPlan, ResourceKind,
    ResourceState, WorkflowFailureReport, WorkflowFailureStatus, WorkflowMode, WorkflowPort,
    WorkflowResult, write_failure_report,
};
use repo_sandbox_core::build::{BuiltImage, ImageRef};
use repo_sandbox_core::config::Platform;
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
        let cancellation = ProcessCancellation;
        let state = repository.join(".repo-sandbox");
        let task_id = task_id();
        let repository_id = repository_id(&repository)?;
        let report_path = plan
            .request
            .report
            .clone()
            .unwrap_or_else(|| state.join("reports").join(format!("{task_id}.json")));
        let _report_reservation = ReportReservation::create(&report_path)?;
        let result = (|| {
            validate_outputs(plan)?;
            let _oci_reservation = plan
                .request
                .oci_layout
                .as_deref()
                .map(OciReservation::create)
                .transpose()?;
            let journal = ManifestJournal::create(&state, &task_id)?;
            let registry = plan.template.execution.registry.as_ref();
            if plan.request.push && registry.is_none() {
                return Err(AppError::Configuration(
                    "--push requires execution.registry.repository in the central profile".into(),
                ));
            }
            preflight(plan, &repository, &cancellation)?;
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
                state: ResourceState::Registered,
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
                .map_err(|error| AppError::Environment(format!("snapshot: {error}")))?;
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
                    &cancellation,
                )
                .map_err(|error| AppError::Environment(format!("environment image: {error}")))?;
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
                        repository_id: &repository_id,
                        created: "1970-01-01T00:00:00Z",
                        repository: image_repository,
                        options: TaskImageOptions {
                            progress: Progress::Plain,
                            output: ImageOutput::Load,
                            ..TaskImageOptions::default()
                        },
                    },
                    &cancellation,
                )
                .map_err(|error| AppError::Environment(format!("task image: {error}")))?;
            journal.append(&[CleanCandidate {
                task_id: task_id.clone(),
                repository_id: repository_id.clone(),
                kind: ResourceKind::Image,
                identifier: task_image.image.digest.to_string(),
                owner: task_image.identity.oci_value(),
                state: ResourceState::Registered,
            }])?;

            let execution = &plan.template.execution;
            let secret_root = tempfile::Builder::new()
                .prefix("repo-sandbox-secrets-")
                .tempdir()
                .map_err(environment("create runtime secret directory"))?;
            let mut secret_mounts = Vec::new();
            for name in &execution.secret_environment {
                let value = std::env::var_os(name).ok_or_else(|| {
                    AppError::Configuration(format!(
                        "required secret environment `{name}` is not set"
                    ))
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
                repository_id: repository_id.clone(),
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
                            state: ResourceState::Registered,
                        }])
                        .map_err(|error| error.to_string())
                })
                .map_err(|error| AppError::Configuration(error.to_string()))?;

            let failed = report.status != RunStatus::Succeeded;
            if report.cleanup == repo_sandbox_core::runner::CleanupResult::RetainedOnFailure {
                materialized.retain_on_failure();
                if !materialized.is_automatically_cleaned()
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
                        state: ResourceState::Retained,
                    }])?;
                }
            }
            if let Some(container) = &report.container_id {
                journal.append(&[CleanCandidate {
                    task_id: task_id.clone(),
                    repository_id: repository_id.clone(),
                    kind: ResourceKind::Container,
                    identifier: container.clone(),
                    owner: task_id.clone(),
                    state: if report.cleanup
                        == repo_sandbox_core::runner::CleanupResult::RetainedOnFailure
                    {
                        ResourceState::Retained
                    } else {
                        ResourceState::Cleaned
                    },
                }])?;
            }

            if !failed && let Some(output) = &plan.request.oci_layout {
                export_verified_oci(
                    plan,
                    &catalog_root,
                    &materialized,
                    &environment_image,
                    &configuration_digest,
                    &repository_id,
                    output,
                    &cancellation,
                )?;
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
                let published_task = if plan.request.platforms.len() > 1 {
                    build_multi_platform_task(
                        plan,
                        &catalog_root,
                        &materialized,
                        &configuration_digest,
                        &repository_id,
                        &repository,
                        &cancellation,
                    )?
                } else {
                    let seeded = seed_registry(
                        &task_image.image.image,
                        &repository,
                        task_image.identity.as_str(),
                    )?;
                    (seeded, task_image.image.clone())
                };
                Some(
                    DockerRegistry::new(SystemRegistryExecutor)
                        .publish(
                            &PublishRequest {
                                source: published_task.0,
                                repository,
                                digest: published_task.1.digest,
                                platform_digests: published_task.1.platform_digests,
                                aliases,
                            },
                            &cancellation,
                        )
                        .map_err(|error| AppError::Environment(error.to_string()))?,
                )
            } else {
                None
            };
            report.published = published.clone();
            annotate_report(&mut report);

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
                | RunStatus::Cancelled { .. }
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
        })();
        if let Err(error) = &result
            && !report_path.exists()
        {
            let failure = WorkflowFailureReport {
                schema_version: 1,
                task_id: task_id.clone(),
                plan_digest: plan.digest.clone(),
                phase: failure_phase(error).into(),
                exit_code: error.exit_code().as_i32(),
                message: error.to_string(),
                cleanup: repo_sandbox_core::runner::CleanupResult::NotNeeded,
                published: None,
                container_id: None,
                source_snapshot: None,
                config: None,
                image: None,
                image_digest: None,
                started_at_unix_ms: 0,
                ended_at_unix_ms: 0,
                duration_ms: 0,
                status: WorkflowFailureStatus {
                    status: "infrastructure_failed",
                    operation: failure_phase(error).into(),
                    message: error.to_string(),
                },
                steps: Vec::new(),
                exported_artifacts: Vec::new(),
                artifact_error: None,
                cleanup_error: None,
            };
            write_failure_report(&failure, &report_path).map_err(|write| {
                AppError::Environment(format!("write failure report: {write}; primary: {error}"))
            })?;
        }
        result
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
        plan.manifest_root = Some(manifests.clone());
        let mut latest = std::collections::BTreeMap::new();
        let mut entries = fs::read_dir(manifests)
            .map_err(environment("read task manifests"))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(environment("read task manifest entry"))?;
        entries.sort();
        for path in entries {
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            let candidates: Vec<CleanCandidate> = serde_json::from_slice(
                &fs::read(&path).map_err(environment("read task manifest"))?,
            )
            .map_err(|error| AppError::Environment(format!("parse {}: {error}", path.display())))?;
            for candidate in candidates {
                latest.insert(
                    (
                        format!("{:?}", candidate.kind),
                        candidate.identifier.clone(),
                    ),
                    candidate,
                );
            }
        }
        for (_, candidate) in latest {
            if candidate.state == ResourceState::Cleaned {
                continue;
            }
            if candidate.repository_id != expected_repository {
                plan.refused.push(format!(
                    "{}: repository owner mismatch",
                    candidate.identifier
                ));
                continue;
            }
            if candidate.kind == ResourceKind::Cache
                && Path::new(&candidate.identifier)
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
                Ok(true) => {
                    result.succeeded.push(candidate.clone());
                    if let Some(root) = &plan.manifest_root {
                        append_cleanup_state(root, candidate)?;
                    }
                }
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

fn validate_outputs(plan: &ExecutionPlan) -> Result<(), AppError> {
    if plan.request.push {
        let policy = plan.template.execution.registry.as_ref().ok_or_else(|| {
            AppError::Configuration(
                "--push requires execution.registry.repository in the central profile".into(),
            )
        })?;
        RegistryRepository::new(&policy.repository).map_err(AppError::Configuration)?;
    }
    let mut seen = std::collections::BTreeSet::new();
    for platform in &plan.request.platforms {
        if !seen.insert(*platform) {
            return Err(AppError::Configuration(format!(
                "duplicate --platform {platform}"
            )));
        }
        if !plan.template.target_platforms.contains(platform) {
            return Err(AppError::Configuration(format!(
                "template {} does not support {platform}",
                plan.template.template_id
            )));
        }
    }
    if plan.request.platforms.len() > 1 && !plan.request.push && plan.request.oci_layout.is_none() {
        return Err(AppError::Configuration(
            "multiple --platform values require --push or --oci-layout".into(),
        ));
    }
    if plan.request.oci_layout.as_ref() == plan.request.report.as_ref() {
        return Err(AppError::Configuration(
            "--oci-layout and --report-path must be different".into(),
        ));
    }
    Ok(())
}

fn annotate_report(report: &mut repo_sandbox_core::runner::RunReport) {
    let (phase, exit_code, message) = match &report.status {
        RunStatus::Succeeded
            if report.cleanup != repo_sandbox_core::runner::CleanupResult::Failed =>
        {
            ("complete", 0, "workflow succeeded".to_owned())
        }
        RunStatus::Succeeded => ("cleanup", 3, "task cleanup failed".to_owned()),
        RunStatus::CommandFailed {
            phase: StepPhase::Build,
            step,
            ..
        } => ("build", 10, format!("build step `{step}` failed")),
        RunStatus::CommandFailed {
            phase: StepPhase::Test,
            step,
            ..
        } => ("test", 11, format!("test step `{step}` failed")),
        RunStatus::Cancelled { phase, step } => (
            match phase {
                Some(StepPhase::Build) => "build",
                Some(StepPhase::Test) => "test",
                None => "runner",
            },
            3,
            step.as_ref()
                .map(|step| format!("step `{step}` cancelled"))
                .unwrap_or_else(|| "workflow cancelled".into()),
        ),
        RunStatus::TimedOut => ("runner", 3, "workflow timed out".to_owned()),
        RunStatus::ResourceExceeded { phase, step, .. } => (
            match phase {
                StepPhase::Build => "build",
                StepPhase::Test => "test",
            },
            3,
            format!("step `{step}` exceeded a resource limit"),
        ),
        RunStatus::InfrastructureFailed { operation, message } => {
            ("runner", 3, format!("{operation}: {message}"))
        }
    };
    report.phase = phase.into();
    report.exit_code = exit_code;
    report.message = message;
}

fn failure_phase(error: &AppError) -> &'static str {
    let message = error.to_string().to_ascii_lowercase();
    if message.contains("preflight") {
        "preflight"
    } else if message.contains("snapshot") || message.contains("git ") {
        "snapshot"
    } else if message.contains("environment image") || message.contains("environment:") {
        "environment_image"
    } else if message.contains("task image") || message.contains("oci task") {
        "task_image"
    } else if message.contains("registry") || message.contains("publish") {
        "publish"
    } else if matches!(error, AppError::BuildFailed(_)) {
        "build"
    } else if matches!(error, AppError::TestFailed(_)) {
        "test"
    } else {
        "orchestration"
    }
}

fn build_multi_platform_task(
    plan: &ExecutionPlan,
    catalog_root: &Path,
    materialized: &crate::snapshot::MaterializedSnapshot,
    configuration_digest: &ConfigurationDigest,
    repository_id: &str,
    repository: &RegistryRepository,
    cancellation: &ProcessCancellation,
) -> Result<(ImageRef, BuiltImage), AppError> {
    let environment_repository = format!("{}-environment", repository.as_str());
    let environment_ref = ImageRef::new(format!(
        "{}:{}",
        environment_repository,
        short_digest(&plan.digest)
    ))
    .map_err(AppError::Configuration)?;
    let environment = BuildKit::new(SystemProcessExecutor)
        .build(
            BuildRequest::environment(
                &plan.template,
                catalog_root,
                environment_ref,
                BuildOptions {
                    progress: Progress::Plain,
                    output: ImageOutput::Push,
                    platforms: plan.request.platforms.clone(),
                    ..BuildOptions::default()
                },
            ),
            cancellation,
        )
        .map_err(|error| AppError::Environment(format!("multi-platform environment: {error}")))?;
    let task = TaskImageBuilder::new(SystemProcessExecutor)
        .build(
            TaskImageRequest {
                environment: &environment,
                materialized,
                template_id: &plan.template.template_id,
                template_version: &plan.template.template_version,
                platform: plan.request.platform,
                configuration_digest,
                repository_id,
                created: "1970-01-01T00:00:00Z",
                repository: repository.as_str(),
                options: TaskImageOptions {
                    progress: Progress::Plain,
                    output: ImageOutput::Push,
                    platforms: plan.request.platforms.clone(),
                    ..TaskImageOptions::default()
                },
            },
            cancellation,
        )
        .map_err(|error| AppError::Environment(format!("multi-platform task image: {error}")))?;
    Ok((task.image.image.clone(), task.image))
}

#[allow(clippy::too_many_arguments)] // Keeps every immutable export input explicit at the port boundary.
fn export_verified_oci(
    plan: &ExecutionPlan,
    catalog_root: &Path,
    materialized: &crate::snapshot::MaterializedSnapshot,
    primary_environment: &BuiltImage,
    configuration_digest: &ConfigurationDigest,
    repository_id: &str,
    output: &Path,
    cancellation: &ProcessCancellation,
) -> Result<(), AppError> {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let temporary = tempfile::Builder::new()
        .prefix(".repo-sandbox-oci-")
        .tempdir_in(parent)
        .map_err(environment("create OCI staging directory"))?;
    let mut layouts = Vec::new();
    for (index, platform) in plan.request.platforms.iter().copied().enumerate() {
        let environment = if platform == plan.request.platform {
            primary_environment.clone()
        } else {
            let image = ImageRef::new(format!(
                "repo-sandbox-env:{}-{index}",
                short_digest(&plan.digest)
            ))
            .map_err(AppError::Configuration)?;
            BuildKit::new(SystemProcessExecutor)
                .build(
                    BuildRequest::environment(
                        &plan.template,
                        catalog_root,
                        image,
                        BuildOptions {
                            progress: Progress::Plain,
                            platforms: vec![platform],
                            ..BuildOptions::default()
                        },
                    ),
                    cancellation,
                )
                .map_err(|error| {
                    AppError::Environment(format!("OCI environment for {platform}: {error}"))
                })?
        };
        let layout = temporary.path().join(format!("platform-{index}"));
        TaskImageBuilder::new(SystemProcessExecutor)
            .build(
                TaskImageRequest {
                    environment: &environment,
                    materialized,
                    template_id: &plan.template.template_id,
                    template_version: &plan.template.template_version,
                    platform,
                    configuration_digest,
                    repository_id,
                    created: "1970-01-01T00:00:00Z",
                    repository: "repo-sandbox-task-oci",
                    options: TaskImageOptions {
                        progress: Progress::Plain,
                        output: ImageOutput::OciDirectory(layout.clone()),
                        platforms: vec![platform],
                        ..TaskImageOptions::default()
                    },
                },
                cancellation,
            )
            .map_err(|error| AppError::Environment(format!("OCI task for {platform}: {error}")))?;
        layouts.push((platform, layout));
    }
    merge_oci_layouts(&layouts, output, temporary.path())
}

fn merge_oci_layouts(
    layouts: &[(Platform, PathBuf)],
    output: &Path,
    temporary_root: &Path,
) -> Result<(), AppError> {
    let merged = temporary_root.join("merged");
    fs::create_dir(&merged).map_err(environment("create merged OCI layout"))?;
    let blobs = merged.join("blobs");
    fs::create_dir(&blobs).map_err(environment("create merged OCI blobs"))?;
    let mut manifests = Vec::new();
    for (platform, layout) in layouts {
        copy_tree(&layout.join("blobs"), &blobs)?;
        let index: serde_json::Value = serde_json::from_slice(
            &fs::read(layout.join("index.json")).map_err(environment("read OCI index"))?,
        )
        .map_err(|error| AppError::Environment(format!("parse OCI index: {error}")))?;
        let descriptors = index
            .get("manifests")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| AppError::Environment("OCI index has no manifests".into()))?;
        if descriptors.len() != 1 {
            return Err(AppError::Environment(format!(
                "single-platform OCI output for {platform} has {} descriptors",
                descriptors.len()
            )));
        }
        let mut descriptor = descriptors[0].clone();
        descriptor["platform"] = serde_json::json!({
            "os": "linux",
            "architecture": match platform {
                Platform::LinuxAmd64 => "amd64",
                Platform::LinuxArm64 => "arm64",
            }
        });
        manifests.push(descriptor);
    }
    fs::write(
        merged.join("oci-layout"),
        b"{\"imageLayoutVersion\":\"1.0.0\"}\n",
    )
    .map_err(environment("write OCI layout marker"))?;
    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": manifests,
    });
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(merged.join("index.json"))
        .map_err(environment("create merged OCI index"))?;
    file.write_all(
        &serde_json::to_vec_pretty(&index).map_err(|error| {
            AppError::Environment(format!("serialize merged OCI index: {error}"))
        })?,
    )
    .map_err(environment("write merged OCI index"))?;
    file.sync_all()
        .map_err(environment("sync merged OCI index"))?;
    drop(file);
    fs::rename(&merged, output).map_err(environment("publish OCI layout atomically"))
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), AppError> {
    for entry in fs::read_dir(source).map_err(environment("read OCI blobs"))? {
        let entry = entry.map_err(environment("read OCI blob entry"))?;
        let target = destination.join(entry.file_name());
        if entry
            .file_type()
            .map_err(environment("inspect OCI blob entry"))?
            .is_dir()
        {
            fs::create_dir_all(&target).map_err(environment("create OCI blob directory"))?;
            copy_tree(&entry.path(), &target)?;
        } else if !target.exists() {
            fs::copy(entry.path(), target).map_err(environment("copy OCI blob"))?;
        }
    }
    Ok(())
}

fn preflight(
    plan: &ExecutionPlan,
    repository: &Path,
    cancellation: &ProcessCancellation,
) -> Result<(), AppError> {
    let mut outputs = Vec::new();
    for args in [
        ["info", "--format", "{{.Architecture}}"].as_slice(),
        ["buildx", "inspect"].as_slice(),
    ] {
        let invocation = ProcessInvocation {
            program: "docker".into(),
            args: args.iter().map(|v| (*v).into()).collect(),
            current_dir: None,
        };
        let output = SystemProcessExecutor
            .execute(&invocation, cancellation)
            .map_err(environment("Docker preflight"))?;
        if output.exit_code != Some(0) {
            return Err(AppError::Environment(format!(
                "Docker preflight failed: {}",
                output.stderr.trim()
            )));
        }
        outputs.push(output.stdout);
    }
    if !matches!(outputs[0].trim(), "amd64" | "x86_64" | "arm64" | "aarch64") {
        return Err(AppError::Environment(format!(
            "Docker preflight reported unsupported host architecture `{}`",
            outputs[0].trim()
        )));
    }
    for platform in &plan.request.platforms {
        if !outputs[1].contains(platform.as_str()) {
            return Err(AppError::Environment(format!(
                "Docker preflight builder does not advertise requested platform {platform}"
            )));
        }
    }
    let free = SystemDoctorProbe
        .available_space(repository)
        .map_err(environment("disk preflight"))?;
    let required = (1024_u64 * 1024 * 1024)
        .max(u64::from(plan.template.execution.resources.temporary_storage_mb) * 2 * 1024 * 1024);
    if free < required {
        return Err(AppError::Environment(format!(
            "disk preflight requires {required} bytes free, found {free} bytes"
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
        let probe_ref = format!(
            "{}:repo-sandbox-preflight-{}",
            policy.repository,
            std::process::id()
        );
        let invocation = ProcessInvocation {
            program: "docker".into(),
            args: vec!["manifest".into(), "inspect".into(), probe_ref],
            current_dir: None,
        };
        let output = SystemProcessExecutor
            .execute(&invocation, cancellation)
            .map_err(environment("registry /v2/ preflight"))?;
        if !registry_probe_authenticated(&output) {
            return Err(AppError::Environment(format!(
                "registry /v2/ preflight authentication or reachability failed: {}",
                output.stderr.trim()
            )));
        }
    }
    Ok(())
}

fn registry_probe_authenticated(output: &crate::buildkit::ProcessOutput) -> bool {
    if output.exit_code == Some(0) {
        return true;
    }
    let stderr = output.stderr.to_ascii_lowercase();
    ["manifest unknown", "no such manifest", "not found"]
        .iter()
        .any(|marker| stderr.contains(marker))
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

#[derive(Debug)]
struct OciReservation {
    path: PathBuf,
}
impl OciReservation {
    fn create(output: &Path) -> Result<Self, AppError> {
        if output.exists() {
            return Err(AppError::Configuration(format!(
                "OCI layout already exists: {}",
                output.display()
            )));
        }
        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(environment("create OCI output parent"))?;
        let name = output
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("layout");
        let path = parent.join(format!(".{name}.repo-sandbox-reservation"));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                AppError::Configuration(format!(
                    "cannot reserve OCI layout {}: {error}",
                    output.display()
                ))
            })?;
        Ok(Self { path })
    }
}
impl Drop for OciReservation {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
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

static CLEAN_EVENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
fn append_cleanup_state(root: &Path, candidate: &CleanCandidate) -> Result<(), AppError> {
    let mut completed = candidate.clone();
    completed.state = ResourceState::Cleaned;
    let sequence = CLEAN_EVENT_SEQUENCE.fetch_add(1, Ordering::SeqCst);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let final_path = root.join(format!(
        "zz-cleanup-{timestamp:020}-{}-{sequence:06}.json",
        std::process::id()
    ));
    let temporary = final_path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(environment("create cleanup state event"))?;
    file.write_all(
        &serde_json::to_vec_pretty(&[completed])
            .map_err(|error| AppError::Environment(error.to_string()))?,
    )
    .map_err(environment("write cleanup state event"))?;
    file.sync_all()
        .map_err(environment("sync cleanup state event"))?;
    fs::rename(temporary, final_path).map_err(environment("publish cleanup state event"))
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
            let repository = docker_output(&[
                "container",
                "inspect",
                "--format",
                "{{ index .Config.Labels \"io.repo-sandbox.repository-id\" }}",
                &candidate.identifier,
            ])?;
            if inspected.exit_code != Some(0) {
                return Ok(false);
            }
            if inspected.stdout.trim() != candidate.owner
                || repository.stdout.trim() != candidate.repository_id
            {
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
            let repository = docker_output(&[
                "image",
                "inspect",
                "--format",
                "{{ index .Config.Labels \"io.repo-sandbox.repository-id\" }}",
                &candidate.identifier,
            ])?;
            if inspected.exit_code != Some(0) {
                return Ok(false);
            }
            if inspected.stdout.trim() != candidate.owner
                || repository.stdout.trim() != candidate.repository_id
            {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_layout(root: &Path, digest: &str) {
        fs::create_dir_all(root.join("blobs/sha256")).unwrap();
        fs::write(root.join("blobs/sha256").join(digest), digest).unwrap();
        fs::write(
            root.join("index.json"),
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "manifests": [{
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": format!("sha256:{digest}"),
                    "size": digest.len()
                }]
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn merges_platform_layouts_into_one_real_oci_index() {
        let temporary = tempfile::tempdir().unwrap();
        let amd64 = temporary.path().join("amd64");
        let arm64 = temporary.path().join("arm64");
        fake_layout(&amd64, &"a".repeat(64));
        fake_layout(&arm64, &"b".repeat(64));
        let output = temporary.path().join("result");
        let staging = temporary.path().join("staging");
        fs::create_dir(&staging).unwrap();

        merge_oci_layouts(
            &[(Platform::LinuxAmd64, amd64), (Platform::LinuxArm64, arm64)],
            &output,
            &staging,
        )
        .unwrap();

        let index: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("index.json")).unwrap()).unwrap();
        let descriptors = index["manifests"].as_array().unwrap();
        assert_eq!(descriptors.len(), 2);
        assert_eq!(descriptors[0]["platform"]["architecture"], "amd64");
        assert_eq!(descriptors[1]["platform"]["architecture"], "arm64");
        assert!(output.join("blobs/sha256").join("a".repeat(64)).is_file());
        assert!(output.join("blobs/sha256").join("b".repeat(64)).is_file());
    }

    #[test]
    fn oci_reservation_rejects_existing_output_without_overwrite() {
        let temporary = tempfile::tempdir().unwrap();
        let output = temporary.path().join("layout");
        fs::create_dir(&output).unwrap();
        let error = OciReservation::create(&output).unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert!(output.is_dir());
    }

    #[test]
    fn registry_probe_requires_reachable_authenticated_v2_response() {
        use crate::buildkit::ProcessOutput;
        let output = |code, stderr: &str| ProcessOutput {
            exit_code: code,
            stdout: String::new(),
            stderr: stderr.into(),
            interrupted: false,
        };
        assert!(registry_probe_authenticated(&output(Some(0), "")));
        assert!(registry_probe_authenticated(&output(
            Some(1),
            "manifest unknown"
        )));
        assert!(!registry_probe_authenticated(&output(
            Some(1),
            "unauthorized: authentication required"
        )));
        assert!(!registry_probe_authenticated(&output(
            None,
            "connection refused"
        )));
    }
}
