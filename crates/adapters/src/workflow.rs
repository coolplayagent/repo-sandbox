//! Concrete composition root for the build/verify/clean application use cases.

use crate::artifacts::{OWNER_MARKER, cleanup_owned_temp_source};
use crate::buildkit::{
    BuildKit, BuildOptions, BuildRequest, CacheConfig, Cancellation, ImageOutput, ProcessExecutor,
    ProcessInvocation, Progress, SystemProcessExecutor,
};
use crate::cancellation::{DeadlineCancellation, ProcessCancellation};
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
use repo_sandbox_core::config::{Platform, RemoteAuthentication};
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
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
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
        let cancellation = plan.deadline.map_or_else(
            || {
                DeadlineCancellation::new(std::time::Duration::from_secs(u64::from(
                    plan.template.execution.timeout_seconds,
                )))
            },
            DeadlineCancellation::at,
        );
        let task_id = task_id();
        let repository_id = repository_id_for_plan(plan, &repository)?;
        let state = state_root(plan, &repository, &repository_id);
        validate_state_root(&repository, &state)?;
        let report_path = plan
            .request
            .report
            .clone()
            .unwrap_or_else(|| state.join("reports").join(format!("{task_id}.json")));
        let _report_reservation = OutputReservation::report(&report_path)?;
        let mut completed_report = None;
        let result = (|| {
            validate_outputs(plan)?;
            let _oci_reservation = plan
                .request
                .oci_layout
                .as_deref()
                .map(OutputReservation::oci)
                .transpose()?;
            prepare_state_layout(&repository, &state)?;
            let journal = ManifestJournal::create(&state, &task_id)?;
            let registry = plan.template.execution.registry.as_ref();
            if plan.request.push && registry.is_none() {
                return Err(AppError::Configuration(
                    "--push requires execution.registry.repository in the central profile".into(),
                ));
            }
            preflight(plan, &repository, &cancellation)?;
            let _workflow_lease = WorkflowLease::shared(&repository.join(".repo-sandbox"))?;
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
                (Some(value), reference) if is_remote_repository(value) => SourceSpec::RemoteGit {
                    repository: value.clone(),
                    git_ref: reference.clone().unwrap_or_else(|| "HEAD".into()),
                },
                _ => SourceSpec::LocalDirectory(repository.clone()),
            };
            let mut materialized = GitSnapshotter::default()
                .with_authentication(external_git_authentication(
                    source_repository(&source),
                    &plan.request.remote_auth,
                )?)
                .create_cancellable(
                    &source,
                    SnapshotOptions {
                        recurse_submodules: plan.request.recurse_submodules,
                        cleanup: CleanupPolicy::Delete,
                    },
                    &cancellation,
                )
                .map_err(|error| AppError::Environment(format!("snapshot: {error}")))?;
            let trusted_catalog = trusted_catalog()?;
            let catalog_root = trusted_catalog.path().to_path_buf();
            let cache_import = cache.join("environment");
            let cache_export = task_cache_export(&cache, &task_id);
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
            // A docker-container builder cannot resolve images loaded only into the host
            // daemon. Export the immutable environment through the client session and pass
            // that OCI layout back as a named context to the same selected builder.
            let environment_export = tempfile::Builder::new()
                .prefix("repo-sandbox-environment-")
                .tempdir()
                .map_err(environment("create environment OCI staging"))?;
            let environment_layout = environment_export.path().join("layout");
            let environment_image = BuildKit::new(SystemProcessExecutor)
                .build(
                    BuildRequest::environment(
                        &plan.template,
                        &catalog_root,
                        environment_ref,
                        owned_environment_options(
                            BuildOptions {
                                progress: Progress::Plain,
                                output: ImageOutput::OciDirectory(environment_layout.clone()),
                                cache: cache_options,
                                ..BuildOptions::default()
                            },
                            &repository_id,
                        ),
                    ),
                    &cancellation,
                )
                .map_err(|error| bounded_error("environment image", error, &cancellation))?;
            if cache_export.exists() {
                rotate_cache_export(&cache, &cache_export, &cache_import, &cancellation)?;
            }

            let configuration_digest =
                ConfigurationDigest::parse(&plan.digest).map_err(AppError::Configuration)?;
            let image_repository = "repo-sandbox-task";
            let task_image = TaskImageBuilder::new(SystemProcessExecutor)
                .build(
                    TaskImageRequest {
                        environment: &environment_image,
                        environment_oci_layout: Some(&environment_layout),
                        identity_environment_digest: None,
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
                .map_err(|error| bounded_error("task image", error, &cancellation))?;
            let execution_image = resolve_local_image_id(&task_image.image, &cancellation)?;
            journal.append(&[owned_task_image_candidate(
                &task_id,
                &repository_id,
                &execution_image,
                task_image.identity.oci_value(),
            )])?;

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
                let value = value.to_string_lossy();
                validate_secret_value(name, value.as_bytes())?;
                let path = secret_root.path().join(name);
                fs::write(&path, value.as_bytes())
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
                image: execution_image,
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
                platform: execution.runner_platform.unwrap_or(plan.request.platform),
                build,
                test,
                resources: RunResources {
                    cpu_count: execution.resources.cpu,
                    memory_mb: execution.resources.memory_mb,
                    temporary_storage_mb: execution.resources.temporary_storage_mb,
                },
                timeout_ms: cancellation
                    .remaining()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX)
                    .max(1),
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

            let mut bookkeeping_errors = Vec::new();
            if report.cleanup == repo_sandbox_core::runner::CleanupResult::RetainedOnFailure {
                materialized.retain_on_failure();
                if !materialized.is_automatically_cleaned() {
                    if let Err(error) = fs::write(materialized.path().join(OWNER_MARKER), &task_id)
                        .map_err(environment("mark retained source"))
                    {
                        bookkeeping_errors.push(error.to_string());
                    } else if let Err(error) = journal.append(&[CleanCandidate {
                        task_id: task_id.clone(),
                        repository_id: repository_id.clone(),
                        kind: ResourceKind::Source,
                        identifier: materialized.path().display().to_string(),
                        owner: task_id.clone(),
                        state: ResourceState::Retained,
                    }]) {
                        bookkeeping_errors.push(error.to_string());
                    }
                }
            }
            if let Some(container) = &report.container_id
                && let Err(error) = journal.append(&[CleanCandidate {
                    task_id: task_id.clone(),
                    repository_id: repository_id.clone(),
                    kind: ResourceKind::Container,
                    identifier: container.clone(),
                    owner: task_id.clone(),
                    state: container_resource_state(report.cleanup),
                }])
            {
                bookkeeping_errors.push(error.to_string());
            }
            apply_bookkeeping_errors(&mut report, bookkeeping_errors);

            annotate_report(&mut report);
            completed_report = Some(report.clone());
            let output_eligible = outputs_allowed(
                &report.status,
                report.cleanup,
                report.cleanup_error.as_deref(),
            );

            if output_eligible && let Some(output) = &plan.request.oci_layout {
                export_verified_oci(
                    plan,
                    &catalog_root,
                    &materialized,
                    &environment_image,
                    &environment_layout,
                    &task_image.image,
                    &configuration_digest,
                    &repository_id,
                    output,
                    &cancellation,
                )?;
            }

            let published = if plan.request.push && output_eligible {
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
                        &environment_image,
                        &task_image.image,
                        &configuration_digest,
                        &repository_id,
                        &repository,
                        &cancellation,
                    )?
                } else {
                    let seeded = seed_registry(
                        &task_image.image.image,
                        &repository,
                        &task_image.image.digest,
                        &cancellation,
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
                        .map_err(|error| bounded_error("publish", error, &cancellation))?,
                )
            } else {
                None
            };
            report.published = published.clone();
            annotate_report(&mut report);
            completed_report = Some(report.clone());

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
            if let Some(mut report) = completed_report {
                if matches!(report.status, RunStatus::Succeeded) {
                    let phase = failure_phase(error);
                    report.phase = phase.into();
                    report.exit_code = error.exit_code().as_i32();
                    report.message = error.to_string();
                    report.status = RunStatus::InfrastructureFailed {
                        operation: phase.into(),
                        message: error.to_string(),
                    };
                }
                write_report_json(&report, &report_path).map_err(|write| {
                    AppError::Environment(format!(
                        "write failure report: {write}; primary: {error}"
                    ))
                })?;
                return result;
            }
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
        let local_repository_id = repository_id(&repository)?;
        let state = repository.join(".repo-sandbox");
        validate_state_root(&repository, &state)?;
        let local_manifests = state.join("tasks");
        let mut stores = vec![(local_manifests.clone(), local_repository_id.clone())];
        if request.all {
            let remotes = state.join("remotes");
            if remotes.is_dir() {
                let mut entries = fs::read_dir(&remotes)
                    .map_err(environment("read remote state stores"))?
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(environment("read remote state store entry"))?;
                entries.sort();
                for remote in entries {
                    let Some(name) = remote.file_name().and_then(|value| value.to_str()) else {
                        continue;
                    };
                    if name.len() != 64 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        continue;
                    }
                    validate_state_root(&repository, &remote)?;
                    stores.push((remote.join("tasks"), format!("sha256:{name}")));
                }
            }
        }
        let mut plan = CleanPlan {
            lease_path: Some(state.join(".workflow.lock")),
            ..CleanPlan::default()
        };
        let mut latest = std::collections::BTreeMap::new();
        for (manifests, expected_repository) in stores {
            if !manifests.is_dir() {
                continue;
            }
            if plan.manifest_root.is_none() {
                plan.manifest_root = Some(manifests.clone());
            }
            plan.journal_roots
                .insert(expected_repository.clone(), manifests.clone());
            let mut entries = fs::read_dir(&manifests)
                .map_err(environment("read task manifests"))?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(environment("read task manifest entry"))?;
            entries.sort_by_key(|path| journal_event_order(path));
            for path in entries {
                if path.extension().and_then(|v| v.to_str()) != Some("json") {
                    continue;
                }
                let candidates: Vec<CleanCandidate> = serde_json::from_slice(
                    &fs::read(&path).map_err(environment("read task manifest"))?,
                )
                .map_err(|error| {
                    AppError::Environment(format!("parse {}: {error}", path.display()))
                })?;
                for candidate in candidates {
                    if candidate.repository_id != expected_repository {
                        plan.refused.push(format!(
                            "{}: repository owner mismatch for trusted store",
                            candidate.identifier
                        ));
                        continue;
                    }
                    latest.insert(
                        (
                            candidate.repository_id.clone(),
                            format!("{:?}", candidate.kind),
                            candidate.identifier.clone(),
                        ),
                        candidate,
                    );
                }
            }
        }
        for (_, candidate) in latest {
            if candidate.state == ResourceState::Cleaned {
                continue;
            }
            let Some(manifest_root) = plan.journal_roots.get(&candidate.repository_id) else {
                plan.refused
                    .push(format!("{}: untrusted journal store", candidate.identifier));
                continue;
            };
            let owned_state = manifest_root.parent().expect("tasks has parent");
            if candidate.kind == ResourceKind::Cache
                && Path::new(&candidate.identifier) != owned_state.join("cache")
            {
                plan.refused
                    .push(format!("{}: cache boundary mismatch", candidate.identifier));
                continue;
            }
            if candidate.kind == ResourceKind::Source && !trusted_source_path(&candidate.identifier)
            {
                plan.refused.push(format!(
                    "{}: source boundary mismatch",
                    candidate.identifier
                ));
                continue;
            }
            if candidate.kind == ResourceKind::Image && !(request.all || request.include_images) {
                plan.refused
                    .push(format!("{}: images not requested", candidate.identifier));
                continue;
            }
            if candidate.kind == ResourceKind::Cache && !(request.all || request.include_cache) {
                plan.refused
                    .push(format!("{}: cache not requested", candidate.identifier));
                continue;
            }
            plan.candidates.push(candidate);
        }
        Ok(plan)
    }

    fn execute(&self, plan: &CleanPlan, dry_run: bool) -> Result<CleanResult, AppError> {
        execute_clean(plan, dry_run, &ProcessCancellation)
    }
}

fn execute_clean(
    plan: &CleanPlan,
    dry_run: bool,
    cancellation: &dyn Cancellation,
) -> Result<CleanResult, AppError> {
    let mut result = CleanResult {
        dry_run,
        skipped: plan.refused.clone(),
        ..CleanResult::default()
    };
    let _lease = if dry_run {
        None
    } else if let Some(path) = &plan.lease_path {
        match WorkflowLease::exclusive(path)? {
            Some(lease) => Some(lease),
            None => {
                result
                    .unfinished
                    .extend(plan.candidates.iter().map(|candidate| {
                        format!(
                            "{}: active workflow holds the repository lease",
                            candidate.identifier
                        )
                    }));
                return Ok(result);
            }
        }
    } else {
        None
    };
    for candidate in &plan.candidates {
        if cancellation.is_cancelled() {
            result.unfinished.push(format!(
                "{}: clean cancelled before candidate",
                candidate.identifier
            ));
            continue;
        }
        if dry_run {
            result.skipped.push(format!(
                "dry-run: {:?} {}",
                candidate.kind, candidate.identifier
            ));
            continue;
        }
        match remove_candidate(candidate, cancellation) {
            Ok(RemovalOutcome::Removed) => {
                result.succeeded.push(candidate.clone());
                let root = plan
                    .journal_roots
                    .get(&candidate.repository_id)
                    .or(plan.manifest_root.as_ref());
                if let Some(root) = root
                    && let Err(error) = append_cleanup_state(root, candidate)
                {
                    result.failed.push(format!(
                        "{}: removed but failed to record cleanup state: {error}",
                        candidate.identifier
                    ));
                }
            }
            Ok(RemovalOutcome::Absent) => result.absent.push(candidate.identifier.clone()),
            Ok(RemovalOutcome::Referenced) => result
                .unfinished
                .push(format!("{}: still referenced", candidate.identifier)),
            Err(error) => result
                .failed
                .push(format!("{}: {error}", candidate.identifier)),
        }
    }
    Ok(result)
}

struct WorkflowLease {
    _file: fs::File,
}

impl WorkflowLease {
    fn open(path: &Path) -> Result<fs::File, AppError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(environment("create workflow lease directory"))?;
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(environment("open workflow lease"))
    }

    fn shared(state: &Path) -> Result<Self, AppError> {
        let file = Self::open(&state.join(".workflow.lock"))?;
        file.try_lock_shared().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => {
                AppError::Environment("clean is active for this repository".into())
            }
            std::fs::TryLockError::Error(error) => {
                AppError::Environment(format!("lock workflow lease: {error}"))
            }
        })?;
        Ok(Self { _file: file })
    }

    fn exclusive(path: &Path) -> Result<Option<Self>, AppError> {
        let file = Self::open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(AppError::Environment(format!(
                "lock clean execution lease: {error}"
            ))),
        }
    }
}

fn validate_outputs(plan: &ExecutionPlan) -> Result<(), AppError> {
    if let Some(repository) = plan.request.repository.as_deref()
        && is_remote_repository(repository)
    {
        validate_remote_repository(repository)?;
    }
    if plan.request.git_ref.is_some()
        && !plan
            .request
            .repository
            .as_deref()
            .is_some_and(is_remote_repository)
    {
        return Err(AppError::Configuration(
            "--git-ref is supported only with a remote repository URL".into(),
        ));
    }
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
    if let (Some(oci), Some(report)) = (&plan.request.oci_layout, &plan.request.report)
        && normalized_output_path(oci)? == normalized_output_path(report)?
    {
        return Err(AppError::Configuration(
            "--oci-layout and --report-path must be different".into(),
        ));
    }
    Ok(())
}

fn normalized_output_path(path: &Path) -> Result<PathBuf, AppError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(environment("resolve output path"))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return Err(AppError::Configuration(format!(
                        "output path escapes its filesystem root: {}",
                        path.display()
                    )));
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
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

fn apply_bookkeeping_errors(
    report: &mut repo_sandbox_core::runner::RunReport,
    errors: Vec<String>,
) {
    if errors.is_empty() {
        return;
    }
    let detail = errors.join("; ");
    report.cleanup_error = Some(match report.cleanup_error.take() {
        Some(existing) => format!("{existing}; journal: {detail}"),
        None => format!("journal: {detail}"),
    });
    if matches!(report.status, RunStatus::Succeeded) {
        report.cleanup = repo_sandbox_core::runner::CleanupResult::Failed;
    }
}

fn outputs_allowed(
    status: &RunStatus,
    cleanup: repo_sandbox_core::runner::CleanupResult,
    cleanup_error: Option<&str>,
) -> bool {
    *status == RunStatus::Succeeded
        && cleanup != repo_sandbox_core::runner::CleanupResult::Failed
        && cleanup_error.is_none()
}

fn container_resource_state(cleanup: repo_sandbox_core::runner::CleanupResult) -> ResourceState {
    match cleanup {
        repo_sandbox_core::runner::CleanupResult::Removed => ResourceState::Cleaned,
        repo_sandbox_core::runner::CleanupResult::RetainedOnFailure => ResourceState::Retained,
        repo_sandbox_core::runner::CleanupResult::Failed
        | repo_sandbox_core::runner::CleanupResult::NotNeeded => ResourceState::Registered,
    }
}

fn validate_secret_value(name: &str, value: &[u8]) -> Result<(), AppError> {
    if value.is_empty() || value.contains(&0) || value.contains(&b'\r') || value.contains(&b'\n') {
        return Err(AppError::Configuration(format!(
            "secret environment `{name}` must be non-empty and single-line"
        )));
    }
    Ok(())
}

fn failure_phase(error: &AppError) -> &'static str {
    let message = error.to_string().to_ascii_lowercase();
    if message.contains("preflight") {
        "preflight"
    } else if message.contains("environment image") || message.contains("environment:") {
        "environment_image"
    } else if message.contains("task image") || message.contains("oci task") {
        "task_image"
    } else if message.contains("snapshot") || message.contains("git ") {
        "snapshot"
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

#[allow(clippy::too_many_arguments)] // Keeps verified and publication identities explicit.
fn build_multi_platform_task(
    plan: &ExecutionPlan,
    catalog_root: &Path,
    materialized: &crate::snapshot::MaterializedSnapshot,
    primary_environment: &BuiltImage,
    verified_task: &BuiltImage,
    configuration_digest: &ConfigurationDigest,
    repository_id: &str,
    repository: &RegistryRepository,
    cancellation: &DeadlineCancellation,
) -> Result<(ImageRef, BuiltImage), AppError> {
    let environment_ref = multi_environment_ref(repository, &plan.digest)?;
    let environment = BuildKit::new(SystemProcessExecutor)
        .build(
            BuildRequest::environment(
                &plan.template,
                catalog_root,
                environment_ref,
                owned_environment_options(
                    BuildOptions {
                        progress: Progress::Plain,
                        output: ImageOutput::Push,
                        platforms: plan.request.platforms.clone(),
                        ..BuildOptions::default()
                    },
                    repository_id,
                ),
            ),
            cancellation,
        )
        .map_err(|error| bounded_error("multi-platform environment", error, cancellation))?;
    let primary_environment_manifest = environment
        .platform_digests
        .iter()
        .find(|item| item.platform == plan.request.platform)
        .ok_or_else(|| {
            AppError::Environment("multi-platform environment omitted the verified platform".into())
        })?;
    if primary_environment_manifest.digest != primary_environment.digest {
        return Err(AppError::Environment(
            "multi-platform environment primary manifest differs from the verified environment"
                .into(),
        ));
    }
    let mut task_manifests = Vec::new();
    let mut sources = Vec::new();
    for platform in &plan.request.platforms {
        let environment_manifest = environment
            .platform_digests
            .iter()
            .find(|item| item.platform == *platform)
            .ok_or_else(|| {
                AppError::Environment(format!("multi-platform environment omitted {platform}"))
            })?;
        let platform_environment = platform_environment(
            primary_environment,
            &environment,
            environment_manifest,
            *platform == plan.request.platform,
        );
        let task = TaskImageBuilder::new(SystemProcessExecutor)
            .build(
                TaskImageRequest {
                    environment: &platform_environment,
                    environment_oci_layout: None,
                    identity_environment_digest: None,
                    materialized,
                    template_id: &plan.template.template_id,
                    template_version: &plan.template.template_version,
                    platform: *platform,
                    configuration_digest,
                    repository_id,
                    created: "1970-01-01T00:00:00Z",
                    repository: repository.as_str(),
                    options: TaskImageOptions {
                        progress: Progress::Plain,
                        output: ImageOutput::Push,
                        platforms: vec![*platform],
                        ..TaskImageOptions::default()
                    },
                },
                cancellation,
            )
            .map_err(|error| {
                bounded_error(
                    &format!("multi-platform task image for {platform}"),
                    error,
                    cancellation,
                )
            })?;
        if *platform == plan.request.platform {
            verify_primary_digest(&task.image, verified_task, "multi-platform task")?;
        }
        sources.push(format!("{}@{}", task.image.image, task.image.digest));
        task_manifests.push(repo_sandbox_core::build::PlatformDigest {
            platform: *platform,
            digest: task.image.digest,
        });
    }
    let index = ImageRef::new(format!(
        "{}:multi-{}",
        repository.as_str(),
        short_digest(&plan.digest)
    ))
    .map_err(AppError::Configuration)?;
    let digest = create_multi_platform_index(&index, &sources, cancellation)?;
    Ok((
        index.clone(),
        BuiltImage {
            image: index,
            digest,
            platform_digests: task_manifests,
        },
    ))
}

#[allow(clippy::too_many_arguments)] // Keeps every immutable export input explicit at the port boundary.
fn export_verified_oci(
    plan: &ExecutionPlan,
    catalog_root: &Path,
    materialized: &crate::snapshot::MaterializedSnapshot,
    primary_environment: &BuiltImage,
    primary_environment_layout: &Path,
    verified_task: &BuiltImage,
    configuration_digest: &ConfigurationDigest,
    repository_id: &str,
    output: &Path,
    cancellation: &DeadlineCancellation,
) -> Result<(), AppError> {
    let temporary = create_oci_staging(output)?;
    let mut layouts = Vec::new();
    for (index, platform) in plan.request.platforms.iter().copied().enumerate() {
        let environment_layout;
        let (environment, environment_context) = if platform == plan.request.platform {
            (primary_environment.clone(), primary_environment_layout)
        } else {
            environment_layout = temporary.path().join(format!("environment-{index}"));
            let image = ImageRef::new(format!(
                "repo-sandbox-env:{}-{index}",
                short_digest(&plan.digest)
            ))
            .map_err(AppError::Configuration)?;
            (
                BuildKit::new(SystemProcessExecutor)
                    .build(
                        BuildRequest::environment(
                            &plan.template,
                            catalog_root,
                            image,
                            owned_environment_options(
                                BuildOptions {
                                    progress: Progress::Plain,
                                    output: ImageOutput::OciDirectory(environment_layout.clone()),
                                    platforms: vec![platform],
                                    ..BuildOptions::default()
                                },
                                repository_id,
                            ),
                        ),
                        cancellation,
                    )
                    .map_err(|error| {
                        bounded_error(
                            &format!("OCI environment for {platform}"),
                            error,
                            cancellation,
                        )
                    })?,
                environment_layout.as_path(),
            )
        };
        let layout = temporary.path().join(format!("platform-{index}"));
        let exported = TaskImageBuilder::new(SystemProcessExecutor)
            .build(
                TaskImageRequest {
                    environment: &environment,
                    environment_oci_layout: Some(environment_context),
                    identity_environment_digest: None,
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
            .map_err(|error| {
                bounded_error(&format!("OCI task for {platform}"), error, cancellation)
            })?;
        if platform == plan.request.platform {
            verify_primary_digest(&exported.image, verified_task, "OCI task")?;
        }
        layouts.push((platform, layout));
    }
    merge_oci_layouts(&layouts, output, temporary.path())
}

fn verify_primary_digest(
    exported: &BuiltImage,
    verified: &BuiltImage,
    operation: &str,
) -> Result<(), AppError> {
    if exported.digest != verified.digest {
        Err(AppError::Environment(format!(
            "{operation} primary manifest differs from the image verified by the runner"
        )))
    } else {
        Ok(())
    }
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
    cancellation: &DeadlineCancellation,
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
            .map_err(|error| bounded_error("Docker preflight", error, cancellation))?;
        if cancellation.expired() {
            return Err(AppError::Environment(
                "workflow timeout during Docker preflight".into(),
            ));
        }
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
            .map_err(|error| bounded_error("registry /v2/ preflight", error, cancellation))?;
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
    if [
        "unauthorized",
        "denied",
        "credential",
        "connection",
        "timeout",
        "tls",
    ]
    .iter()
    .any(|marker| stderr.contains(marker))
    {
        return false;
    }
    ["manifest unknown", "no such manifest"]
        .iter()
        .any(|marker| stderr.contains(marker))
}

fn repository_path(plan: &ExecutionPlan) -> Result<PathBuf, AppError> {
    if plan
        .request
        .repository
        .as_deref()
        .is_some_and(is_remote_repository)
    {
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

fn trusted_catalog() -> Result<tempfile::TempDir, AppError> {
    const ASSETS: &[(&str, &[u8])] = &[
        (
            "templates/rust-bazel/context/Dockerfile",
            include_bytes!("../../../templates/rust-bazel/context/Dockerfile"),
        ),
        (
            "templates/components/base-tools/context/Dockerfile",
            include_bytes!("../../../templates/components/base-tools/context/Dockerfile"),
        ),
        (
            "templates/components/bazel/context/Dockerfile",
            include_bytes!("../../../templates/components/bazel/context/Dockerfile"),
        ),
        (
            "templates/components/rust/context/Dockerfile",
            include_bytes!("../../../templates/components/rust/context/Dockerfile"),
        ),
    ];
    let catalog = tempfile::Builder::new()
        .prefix("repo-sandbox-catalog-")
        .tempdir()
        .map_err(environment("create trusted central catalog"))?;
    for (relative, bytes) in ASSETS {
        let path = catalog.path().join(relative);
        fs::create_dir_all(path.parent().expect("catalog asset has parent"))
            .map_err(environment("create trusted catalog asset directory"))?;
        fs::write(path, bytes).map_err(environment("write trusted catalog asset"))?;
    }
    Ok(catalog)
}

pub fn is_remote_repository(value: &str) -> bool {
    value.contains("://") || is_scp_remote(value)
}

pub fn validate_remote_repository(value: &str) -> Result<(), AppError> {
    if value.contains("://") && value.contains(['?', '#']) {
        return Err(AppError::Configuration(
            "remote repository URLs must not contain query parameters or fragments; use explicit external credential options".into(),
        ));
    }
    Ok(())
}

fn is_scp_remote(value: &str) -> bool {
    value.split_once(':').is_some_and(|(authority, path)| {
        authority
            .split_once('@')
            .is_some_and(|(user, host)| !user.is_empty() && !host.is_empty())
            && !path.is_empty()
            && !path.starts_with(['\\', '/'])
    })
}
fn source_repository(source: &SourceSpec) -> &str {
    match source {
        SourceSpec::RemoteGit { repository, .. } => repository,
        SourceSpec::LocalDirectory(_) => "",
    }
}

/// Map opt-in fixture credentials to external references. Values are never
/// copied into configuration, argv, logs, or reports.
pub fn external_git_authentication(
    repository: &str,
    auth: &RemoteAuthentication,
) -> Result<GitAuthentication, AppError> {
    let https_modes = usize::from(auth.https_token_environment.is_some())
        + usize::from(auth.https_credential_helper);
    if https_modes > 1 || (auth.ssh_private_key.is_some() && auth.ssh_agent) {
        return Err(AppError::Configuration(
            "remote authentication methods are mutually exclusive per transport".into(),
        ));
    }
    if let Some(name) = &auth.https_token_environment {
        let valid = !name.is_empty()
            && name.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
            });
        if !valid {
            return Err(AppError::Configuration(
                "--git-https-token-env must name a POSIX environment variable".into(),
            ));
        }
    }
    if auth
        .https_username
        .as_deref()
        .is_some_and(|value| value.is_empty() || value.contains(['\r', '\n']))
    {
        return Err(AppError::Configuration(
            "--git-https-username must be non-empty and single-line".into(),
        ));
    }
    if is_scp_remote(repository) || repository.starts_with("ssh://") {
        if https_modes > 0 || auth.https_username.is_some() {
            return Err(AppError::Configuration(
                "HTTPS authentication options cannot be used with an SSH remote".into(),
            ));
        }
        if let Some(private_key) = &auth.ssh_private_key {
            return Ok(GitAuthentication::SshKey {
                private_key: private_key.clone(),
                known_hosts: auth.ssh_known_hosts.clone(),
            });
        }
        if auth.ssh_agent {
            return Ok(GitAuthentication::SshAgent {
                known_hosts: auth.ssh_known_hosts.clone(),
            });
        }
        if auth.ssh_known_hosts.is_some() {
            return Err(AppError::Configuration(
                "--git-ssh-known-hosts requires --git-ssh-private-key or --git-ssh-agent".into(),
            ));
        }
        return Ok(GitAuthentication::None);
    }
    if repository.starts_with("https://") {
        if auth.ssh_private_key.is_some() || auth.ssh_known_hosts.is_some() || auth.ssh_agent {
            return Err(AppError::Configuration(
                "SSH authentication options cannot be used with an HTTPS remote".into(),
            ));
        }
        if let Some(environment) = &auth.https_token_environment {
            return Ok(GitAuthentication::HttpsToken {
                username: auth.https_username.clone().unwrap_or_else(|| "git".into()),
                token: ExternalSecret::Environment(environment.clone()),
            });
        }
        if auth.https_credential_helper {
            return Ok(GitAuthentication::HttpsCredentialHelper);
        }
    }
    if https_modes > 0
        || auth.https_username.is_some()
        || auth.ssh_private_key.is_some()
        || auth.ssh_known_hosts.is_some()
        || auth.ssh_agent
    {
        return Err(AppError::Configuration(
            "remote authentication options require a matching HTTPS or SSH remote".into(),
        ));
    }
    Ok(GitAuthentication::None)
}
fn short_digest(value: &str) -> &str {
    value
        .strip_prefix("sha256:")
        .unwrap_or(value)
        .get(..24)
        .unwrap_or(value)
}

fn task_cache_export(cache: &Path, task_id: &str) -> PathBuf {
    cache.join(format!("environment-next-{task_id}"))
}

fn owned_task_image_candidate(
    task_id: &str,
    repository_id: &str,
    execution_image: &ImageRef,
    owner: String,
) -> CleanCandidate {
    CleanCandidate {
        task_id: task_id.into(),
        repository_id: repository_id.into(),
        kind: ResourceKind::Image,
        identifier: execution_image.to_string(),
        owner,
        state: ResourceState::Registered,
    }
}

#[cfg(test)]
fn owned_environment_image_candidate(
    task_id: &str,
    repository_id: &str,
    execution_image: &ImageRef,
    plan_digest: &str,
) -> CleanCandidate {
    CleanCandidate {
        task_id: task_id.into(),
        repository_id: repository_id.into(),
        kind: ResourceKind::Image,
        identifier: execution_image.to_string(),
        owner: plan_digest.into(),
        state: ResourceState::Registered,
    }
}

fn owned_environment_options(mut options: BuildOptions, repository_id: &str) -> BuildOptions {
    options
        .build_args
        .insert("REPO_SANDBOX_REPOSITORY_ID".into(), repository_id.into());
    options
}

fn rotate_cache_export(
    cache: &Path,
    export: &Path,
    current: &Path,
    cancellation: &DeadlineCancellation,
) -> Result<(), AppError> {
    let lock_path = cache.join(".rotation.lock");
    let file = WorkflowLease::open(&lock_path)?;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if !cancellation.is_cancelled() => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(AppError::Environment(
                    "workflow cancelled while waiting to rotate cache".into(),
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(AppError::Environment(format!(
                    "lock cache rotation: {error}"
                )));
            }
        }
    }
    let backup = cache.join(format!("environment-previous-{}", task_id()));
    if current.exists() {
        fs::rename(current, &backup).map_err(environment("preserve current cache"))?;
    }
    if let Err(error) = fs::rename(export, current) {
        if backup.exists() {
            let _ = fs::rename(&backup, current);
        }
        return Err(AppError::Environment(format!(
            "rotate cache export: {error}"
        )));
    }
    if backup.exists() {
        fs::remove_dir_all(backup).map_err(environment("remove previous cache"))?;
    }
    Ok(())
}

fn multi_environment_ref(
    repository: &RegistryRepository,
    plan_digest: &str,
) -> Result<ImageRef, AppError> {
    ImageRef::new(format!(
        "{}:environment-{}",
        repository.as_str(),
        short_digest(plan_digest)
    ))
    .map_err(AppError::Configuration)
}

fn create_multi_platform_index(
    target: &ImageRef,
    sources: &[String],
    cancellation: &dyn Cancellation,
) -> Result<repo_sandbox_core::build::ImageDigest, AppError> {
    let mut args = vec![
        "buildx".into(),
        "imagetools".into(),
        "create".into(),
        "--tag".into(),
        target.to_string(),
    ];
    args.extend(sources.iter().cloned());
    let run = |args: Vec<String>, operation: &'static str| {
        let output = SystemProcessExecutor
            .execute(
                &ProcessInvocation {
                    program: "docker".into(),
                    args,
                    current_dir: None,
                },
                cancellation,
            )
            .map_err(environment(operation))?;
        if output.interrupted || output.exit_code != Some(0) {
            return Err(AppError::Environment(format!(
                "{operation}: {}",
                output.stderr.trim()
            )));
        }
        Ok(output)
    };
    run(args, "create multi-platform task index")?;
    let inspected = run(
        vec![
            "buildx".into(),
            "imagetools".into(),
            "inspect".into(),
            target.to_string(),
        ],
        "inspect multi-platform task index",
    )?;
    let digest = inspected
        .stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Digest:").map(str::trim))
        .ok_or_else(|| AppError::Environment("multi-platform index omitted its digest".into()))?;
    repo_sandbox_core::build::ImageDigest::new(digest).map_err(AppError::Environment)
}

fn platform_environment(
    primary: &BuiltImage,
    index: &BuiltImage,
    manifest: &repo_sandbox_core::build::PlatformDigest,
    is_primary: bool,
) -> BuiltImage {
    if is_primary {
        primary.clone()
    } else {
        BuiltImage {
            image: index.image.clone(),
            digest: manifest.digest.clone(),
            platform_digests: vec![manifest.clone()],
        }
    }
}

fn resolve_local_image_id(
    image: &BuiltImage,
    cancellation: &DeadlineCancellation,
) -> Result<ImageRef, AppError> {
    resolve_local_image_id_with(&SystemProcessExecutor, image, cancellation)
}

fn resolve_local_image_id_with(
    executor: &dyn ProcessExecutor,
    image: &BuiltImage,
    cancellation: &dyn Cancellation,
) -> Result<ImageRef, AppError> {
    let output = executor
        .execute(
            &ProcessInvocation {
                program: "docker".into(),
                args: vec![
                    "image".into(),
                    "inspect".into(),
                    "--format={{.Id}}".into(),
                    image.image.to_string(),
                ],
                current_dir: None,
            },
            cancellation,
        )
        .map_err(environment("resolve immutable local task image ID"))?;
    if output.interrupted || output.exit_code != Some(0) {
        return Err(AppError::Environment(format!(
            "resolve immutable local task image ID failed: {}",
            output.stderr.trim()
        )));
    }
    let id = output.stdout.trim();
    repo_sandbox_core::build::ImageDigest::new(id)
        .map_err(|_| AppError::Environment("Docker returned an invalid task image ID".into()))?;
    ImageRef::new(id).map_err(AppError::Configuration)
}

fn create_oci_staging(output: &Path) -> Result<tempfile::TempDir, AppError> {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(environment("create OCI output parent"))?;
    tempfile::Builder::new()
        .prefix(".repo-sandbox-oci-")
        .tempdir_in(parent)
        .map_err(environment("create OCI staging directory"))
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

fn repository_id_for_plan(plan: &ExecutionPlan, local_root: &Path) -> Result<String, AppError> {
    match plan
        .request
        .repository
        .as_deref()
        .filter(|value| is_remote_repository(value))
    {
        Some(remote) => {
            let normalized = normalize_remote_identity(remote)?;
            let mut hasher = Sha256::new();
            hasher.update(b"repo-sandbox-remote-v1\0");
            hasher.update(normalized.as_bytes());
            Ok(format!("sha256:{:x}", hasher.finalize()))
        }
        None => repository_id(local_root),
    }
}

fn normalize_remote_identity(remote: &str) -> Result<String, AppError> {
    validate_remote_repository(remote)?;
    let value = remote.trim().trim_end_matches('/');
    if value.is_empty() {
        return Err(AppError::Configuration("remote repository is empty".into()));
    }
    if let Some((scheme, remainder)) = value.split_once("://") {
        if scheme.eq_ignore_ascii_case("file") {
            let path = remainder
                .split(['?', '#'])
                .next()
                .unwrap_or(remainder)
                .replace('\\', "/");
            if path.is_empty() {
                return Err(AppError::Configuration(
                    "file remote repository must include a path".into(),
                ));
            }
            return Ok(format!("file://{}", path.trim_end_matches('/')));
        }
        let authority_path = remainder.split(['?', '#']).next().unwrap_or(remainder);
        let (authority, path) = authority_path
            .split_once('/')
            .unwrap_or((authority_path, ""));
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        if host.is_empty() || path.is_empty() {
            return Err(AppError::Configuration(
                "remote repository URL must include a host and path".into(),
            ));
        }
        return Ok(format!(
            "{}://{}/{}",
            scheme.to_ascii_lowercase(),
            host.to_ascii_lowercase(),
            path.trim_end_matches('/')
        ));
    }
    if let Some((user_host, path)) = value.split_once(':')
        && let Some((_, host)) = user_host.rsplit_once('@')
        && !host.is_empty()
        && !path.is_empty()
    {
        return Ok(format!(
            "ssh://{}/{}",
            host.to_ascii_lowercase(),
            path.trim_start_matches('/').trim_end_matches('/')
        ));
    }
    Err(AppError::Configuration(format!(
        "unsupported remote repository `{remote}`"
    )))
}

fn state_root(plan: &ExecutionPlan, local_root: &Path, repository_id: &str) -> PathBuf {
    let state = local_root.join(".repo-sandbox");
    if plan
        .request
        .repository
        .as_deref()
        .is_some_and(is_remote_repository)
    {
        state.join("remotes").join(
            repository_id
                .strip_prefix("sha256:")
                .unwrap_or(repository_id),
        )
    } else {
        state
    }
}

fn validate_state_root(repository: &Path, state: &Path) -> Result<(), AppError> {
    let base = repository.join(".repo-sandbox");
    if !state.starts_with(&base) {
        return Err(AppError::Environment(
            "workflow state store escapes the trusted state root".into(),
        ));
    }
    let mut paths = vec![base.clone(), base.join("remotes")];
    if state != base {
        paths.push(base.join("remotes"));
        paths.push(state.to_path_buf());
    }
    for leaf in ["tasks", "cache", "reports", "artifacts"] {
        paths.push(state.join(leaf));
    }
    for path in paths {
        validate_state_component(repository, &path)?;
    }
    Ok(())
}

fn prepare_state_layout(repository: &Path, state: &Path) -> Result<(), AppError> {
    // Complete the read-only validation pass before creating any component.
    validate_state_root(repository, state)?;
    let base = repository.join(".repo-sandbox");
    let mut paths = vec![base.clone()];
    if state != base {
        paths.push(base.join("remotes"));
        paths.push(state.to_path_buf());
    }
    for leaf in ["tasks", "cache", "reports", "artifacts"] {
        paths.push(state.join(leaf));
    }
    for path in paths {
        if !path.exists() {
            fs::create_dir(&path).map_err(environment("create workflow state component"))?;
        }
        validate_state_component(repository, &path)?;
    }
    Ok(())
}

fn validate_state_component(repository: &Path, path: &Path) -> Result<(), AppError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(environment("inspect workflow state component")(error)),
    };
    if is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(AppError::Environment(format!(
            "workflow state component must be a real directory: {}",
            path.display()
        )));
    }
    let canonical_repository = repository
        .canonicalize()
        .map_err(environment("resolve repository state boundary"))?;
    let canonical = path
        .canonicalize()
        .map_err(environment("resolve workflow state component"))?;
    if !canonical.starts_with(canonical_repository) {
        return Err(AppError::Environment(format!(
            "workflow state component escapes the repository: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}
fn environment(operation: &'static str) -> impl FnOnce(std::io::Error) -> AppError {
    move |error| AppError::Environment(format!("{operation}: {error}"))
}

fn bounded_error(
    operation: &str,
    error: impl std::fmt::Display,
    cancellation: &DeadlineCancellation,
) -> AppError {
    if cancellation.expired() {
        AppError::Environment(format!("workflow timeout during {operation}"))
    } else {
        AppError::Environment(format!("{operation}: {error}"))
    }
}

/// Cross-process reservation for a requested output path. The reservation is
/// kept outside the source repository so it can never alter snapshot identity.
#[derive(Debug)]
pub struct OutputReservation {
    _file: fs::File,
}

impl OutputReservation {
    pub fn oci(output: &Path) -> Result<Self, AppError> {
        if output.exists() {
            return Err(AppError::Configuration(format!(
                "OCI layout already exists: {}",
                output.display()
            )));
        }
        Self::create(output, "OCI layout")
    }

    pub fn report(report: &Path) -> Result<Self, AppError> {
        if report.exists() {
            return Err(AppError::Configuration(format!(
                "report already exists: {}",
                report.display()
            )));
        }
        Self::create(report, "report")
    }

    fn create(output: &Path, description: &str) -> Result<Self, AppError> {
        let absolute = normalized_output_path(output)?;
        let mut digest = Sha256::new();
        digest.update(absolute.to_string_lossy().as_bytes());
        let root = std::env::temp_dir().join("repo-sandbox-output-reservations-v1");
        fs::create_dir_all(&root).map_err(environment("create output reservation directory"))?;
        let path = root.join(format!("{:x}.lock", digest.finalize()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| {
                AppError::Environment(format!(
                    "cannot reserve {description} {}: {error}",
                    output.display()
                ))
            })?;
        file.try_lock().map_err(|error| {
            AppError::Configuration(format!(
                "cannot reserve {description} {}: {error}",
                output.display()
            ))
        })?;
        Ok(Self { _file: file })
    }
}

struct ManifestJournal {
    root: PathBuf,
    task_id: String,
}

fn append_cleanup_state(root: &Path, candidate: &CleanCandidate) -> Result<(), AppError> {
    let mut completed = candidate.clone();
    completed.state = ResourceState::Cleaned;
    let sequence = next_journal_sequence(root)?;
    let final_path = root.join(format!(
        "event-{sequence:020}-cleanup-{}.json",
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
        };
        journal.append(&[])?;
        Ok(journal)
    }

    fn append(&self, candidates: &[CleanCandidate]) -> Result<(), AppError> {
        let sequence = next_journal_sequence(&self.root)?;
        let final_path = self
            .root
            .join(format!("event-{sequence:020}-{}.json", self.task_id));
        let temporary = self.root.join(format!(
            ".event-{sequence:020}-{}.{}.tmp",
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

fn next_journal_sequence(root: &Path) -> Result<u64, AppError> {
    fs::create_dir_all(root).map_err(environment("create journal directory"))?;
    let path = root.join(".sequence");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(environment("open journal sequence"))?;
    file.lock().map_err(environment("lock journal sequence"))?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(environment("read journal sequence"))?;
    let observed = fs::read_dir(root)
        .map_err(environment("scan journal sequence"))?
        .filter_map(Result::ok)
        .filter_map(|entry| new_event_sequence(&entry.path()))
        .max()
        .unwrap_or(0);
    let previous = text.trim().parse::<u64>().unwrap_or(0).max(observed);
    let next = previous.checked_add(1).ok_or_else(|| {
        AppError::Environment("journal sequence exhausted its numeric range".into())
    })?;
    file.set_len(0)
        .map_err(environment("truncate journal sequence"))?;
    file.seek(SeekFrom::Start(0))
        .map_err(environment("seek journal sequence"))?;
    writeln!(file, "{next}").map_err(environment("write journal sequence"))?;
    file.sync_all()
        .map_err(environment("sync journal sequence"))?;
    Ok(next)
}

fn new_event_sequence(path: &Path) -> Option<u64> {
    path.file_name()?
        .to_str()?
        .strip_prefix("event-")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

fn journal_event_order(path: &Path) -> (u8, u128, String) {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_owned();
    if let Some(sequence) = new_event_sequence(path) {
        return (1, u128::from(sequence), name);
    }
    let modified = path
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    (0, modified, name)
}

fn docker_output(
    args: &[&str],
    cancellation: &dyn Cancellation,
) -> Result<crate::buildkit::ProcessOutput, String> {
    let invocation = ProcessInvocation {
        program: "docker".into(),
        args: args.iter().map(|v| (*v).into()).collect(),
        current_dir: None,
    };
    SystemProcessExecutor
        .execute(&invocation, cancellation)
        .map_err(|e| e.to_string())
}

fn seed_registry(
    source: &ImageRef,
    repository: &RegistryRepository,
    digest: &repo_sandbox_core::build::ImageDigest,
    cancellation: &DeadlineCancellation,
) -> Result<ImageRef, AppError> {
    let content = registry_content_ref(repository, digest);
    for args in [
        vec!["image", "tag", source.as_str(), content.as_str()],
        vec!["push", content.as_str()],
    ] {
        let invocation = ProcessInvocation {
            program: "docker".into(),
            args: args.into_iter().map(str::to_owned).collect(),
            current_dir: None,
        };
        let output = SystemProcessExecutor
            .execute(&invocation, cancellation)
            .map_err(|error| bounded_error("registry seed push", error, cancellation))?;
        if cancellation.expired() {
            return Err(AppError::Environment(
                "workflow timeout during registry seed push".into(),
            ));
        }
        if output.exit_code != Some(0) {
            return Err(AppError::Environment(format!(
                "registry seed push failed: {}",
                output.stderr.trim()
            )));
        }
    }
    Ok(content)
}

fn registry_content_ref(
    repository: &RegistryRepository,
    digest: &repo_sandbox_core::build::ImageDigest,
) -> ImageRef {
    repository.tagged(&RegistryTag::for_digest(digest))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemovalOutcome {
    Removed,
    Absent,
    Referenced,
}

fn remove_candidate(
    candidate: &CleanCandidate,
    cancellation: &dyn Cancellation,
) -> Result<RemovalOutcome, String> {
    match candidate.kind {
        ResourceKind::Container => {
            let inspected = docker_output(
                &[
                    "container",
                    "inspect",
                    "--format",
                    "{{ index .Config.Labels \"io.repo-sandbox.task-id\" }}",
                    &candidate.identifier,
                ],
                cancellation,
            )?;
            if inspected.exit_code != Some(0) {
                return if docker_object_absent(&inspected.stderr) {
                    Ok(RemovalOutcome::Absent)
                } else {
                    Err(format!(
                        "inspect owned container: {}",
                        inspected.stderr.trim()
                    ))
                };
            }
            let repository = docker_output(
                &[
                    "container",
                    "inspect",
                    "--format",
                    "{{ index .Config.Labels \"io.repo-sandbox.repository-id\" }}",
                    &candidate.identifier,
                ],
                cancellation,
            )?;
            if repository.exit_code != Some(0) {
                return Err(format!(
                    "inspect container repository owner: {}",
                    repository.stderr.trim()
                ));
            }
            if inspected.stdout.trim() != candidate.owner
                || repository.stdout.trim() != candidate.repository_id
            {
                return Err("owner label mismatch".into());
            }
            let removed = docker_output(
                &["container", "rm", "--force", &candidate.identifier],
                cancellation,
            )?;
            if removed.exit_code == Some(0) {
                Ok(RemovalOutcome::Removed)
            } else {
                Err(removed.stderr)
            }
        }
        ResourceKind::Image => {
            let inspected = docker_output(
                &[
                    "image",
                    "inspect",
                    "--format",
                    "{{ index .Config.Labels \"io.repo-sandbox.owner\" }}",
                    &candidate.identifier,
                ],
                cancellation,
            )?;
            if inspected.exit_code != Some(0) {
                return if docker_object_absent(&inspected.stderr) {
                    Ok(RemovalOutcome::Absent)
                } else {
                    Err(format!("inspect owned image: {}", inspected.stderr.trim()))
                };
            }
            let repository = docker_output(
                &[
                    "image",
                    "inspect",
                    "--format",
                    "{{ index .Config.Labels \"io.repo-sandbox.repository-id\" }}",
                    &candidate.identifier,
                ],
                cancellation,
            )?;
            if repository.exit_code != Some(0) {
                return Err(format!(
                    "inspect image repository owner: {}",
                    repository.stderr.trim()
                ));
            }
            if inspected.stdout.trim() != candidate.owner
                || repository.stdout.trim() != candidate.repository_id
            {
                return Err("image owner label mismatch".into());
            }
            let references = docker_output(
                &[
                    "container",
                    "ls",
                    "--all",
                    "--quiet",
                    "--filter",
                    &format!("ancestor={}", candidate.identifier),
                ],
                cancellation,
            )?;
            if references.exit_code != Some(0) {
                return Err(format!(
                    "inspect image references: {}",
                    references.stderr.trim()
                ));
            }
            if !references.stdout.trim().is_empty() {
                return Ok(RemovalOutcome::Referenced);
            }
            let removed = docker_output(&["image", "rm", &candidate.identifier], cancellation)?;
            if removed.exit_code == Some(0) {
                Ok(RemovalOutcome::Removed)
            } else {
                Err(removed.stderr)
            }
        }
        ResourceKind::Source => {
            let path = PathBuf::from(&candidate.identifier);
            if !path.exists() {
                return Ok(RemovalOutcome::Absent);
            }
            let parent = path.parent().ok_or("source has no parent")?;
            cleanup_owned_temp_source(parent, &path, &candidate.owner)
                .map(|_| RemovalOutcome::Removed)
                .map_err(|e| e.to_string())
        }
        ResourceKind::Cache => {
            let path = PathBuf::from(&candidate.identifier);
            if !path.exists() {
                return Ok(RemovalOutcome::Absent);
            }
            let owner = fs::read_to_string(path.join(OWNER_MARKER)).map_err(|e| e.to_string())?;
            if owner != candidate.owner {
                return Err("cache owner marker mismatch".into());
            }
            fs::remove_dir_all(path)
                .map(|_| RemovalOutcome::Removed)
                .map_err(|e| e.to_string())
        }
        ResourceKind::Builder => {
            Err("builder cleanup requires an exact adapter ownership record".into())
        }
    }
}

fn docker_object_absent(stderr: &str) -> bool {
    let message = stderr.to_ascii_lowercase();
    message.contains("no such container")
        || message.contains("no such image")
        || message.contains("no such object")
}

fn trusted_source_path(identifier: &str) -> bool {
    let path = Path::new(identifier);
    let has_owned_name = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.starts_with("repo-sandbox-source-"));
    has_owned_name
        && path.file_name().is_some_and(|name| name == "source")
        && path.starts_with(std::env::temp_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buildkit::NeverCancelled;

    struct InspectExecutor {
        calls: std::sync::Mutex<Vec<ProcessInvocation>>,
        image_id: String,
    }

    impl ProcessExecutor for InspectExecutor {
        fn execute(
            &self,
            invocation: &ProcessInvocation,
            _cancellation: &dyn Cancellation,
        ) -> std::io::Result<crate::buildkit::ProcessOutput> {
            self.calls.lock().unwrap().push(invocation.clone());
            Ok(crate::buildkit::ProcessOutput {
                exit_code: Some(0),
                stdout: format!("{}\n", self.image_id),
                stderr: String::new(),
                interrupted: false,
            })
        }
    }

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
    fn oci_primary_must_equal_the_runner_verified_manifest() {
        let image = |value: char| BuiltImage {
            image: ImageRef::new("repo-sandbox-task:test").unwrap(),
            digest: repo_sandbox_core::build::ImageDigest::new(format!(
                "sha256:{}",
                value.to_string().repeat(64)
            ))
            .unwrap(),
            platform_digests: Vec::new(),
        };
        assert!(verify_primary_digest(&image('a'), &image('a'), "OCI task").is_ok());
        let error = verify_primary_digest(&image('b'), &image('a'), "OCI task").unwrap_err();
        assert!(error.to_string().contains("runner"));
    }

    #[test]
    fn oci_staging_creates_a_fresh_nested_output_parent() {
        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path().join("reports/task-oci");
        let staging = create_oci_staging(&parent.join("layout")).unwrap();
        assert!(parent.is_dir());
        assert_eq!(staging.path().parent(), Some(parent.as_path()));
    }

    #[test]
    fn cache_exports_are_task_specific_and_rotation_preserves_latest_complete_export() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = temporary.path();
        let current = cache.join("environment");
        let first = task_cache_export(cache, "first");
        let second = task_cache_export(cache, "second");
        assert_ne!(first, second);
        fs::create_dir(&first).unwrap();
        fs::write(first.join("value"), "first").unwrap();
        rotate_cache_export(
            cache,
            &first,
            &current,
            &DeadlineCancellation::new(std::time::Duration::from_secs(1)),
        )
        .unwrap();
        fs::create_dir(&second).unwrap();
        fs::write(second.join("value"), "second").unwrap();
        rotate_cache_export(
            cache,
            &second,
            &current,
            &DeadlineCancellation::new(std::time::Duration::from_secs(1)),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(current.join("value")).unwrap(), "second");
        assert!(!first.exists());
        assert!(!second.exists());
    }

    #[test]
    fn concurrent_cache_rotations_publish_only_complete_task_exports() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = temporary.path().to_path_buf();
        let current = cache.join("environment");
        let first = task_cache_export(&cache, "first-concurrent");
        let second = task_cache_export(&cache, "second-concurrent");
        for (path, value) in [(&first, "first"), (&second, "second")] {
            fs::create_dir(path).unwrap();
            fs::write(path.join("value"), value).unwrap();
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let spawn = |export: PathBuf| {
            let cache = cache.clone();
            let current = current.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                rotate_cache_export(
                    &cache,
                    &export,
                    &current,
                    &DeadlineCancellation::new(std::time::Duration::from_secs(2)),
                )
                .unwrap();
            })
        };
        let first_thread = spawn(first.clone());
        let second_thread = spawn(second.clone());
        barrier.wait();
        first_thread.join().unwrap();
        second_thread.join().unwrap();
        let value = fs::read_to_string(current.join("value")).unwrap();
        assert!(matches!(value.as_str(), "first" | "second"));
        assert!(!first.exists());
        assert!(!second.exists());
        assert_eq!(
            fs::read_dir(&cache)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("environment-previous-"))
                .count(),
            0
        );
    }

    #[test]
    fn multi_platform_environment_stays_in_the_configured_repository() {
        let repository = RegistryRepository::new("registry.test/team/task").unwrap();
        let reference =
            multi_environment_ref(&repository, &format!("sha256:{}", "a".repeat(64))).unwrap();
        assert_eq!(
            reference.as_str(),
            format!("registry.test/team/task:environment-{}", "a".repeat(24))
        );
        assert!(!reference.as_str().contains("task-environment"));
    }

    #[test]
    fn non_primary_task_identity_uses_its_platform_environment_digest() {
        let primary_digest =
            repo_sandbox_core::build::ImageDigest::new(format!("sha256:{}", "a".repeat(64)))
                .unwrap();
        let arm_digest =
            repo_sandbox_core::build::ImageDigest::new(format!("sha256:{}", "b".repeat(64)))
                .unwrap();
        let primary = BuiltImage {
            image: ImageRef::new("repo-sandbox-env:primary").unwrap(),
            digest: primary_digest,
            platform_digests: Vec::new(),
        };
        let index = BuiltImage {
            image: ImageRef::new("registry.test/team/task:environment").unwrap(),
            digest: repo_sandbox_core::build::ImageDigest::new(format!(
                "sha256:{}",
                "c".repeat(64)
            ))
            .unwrap(),
            platform_digests: Vec::new(),
        };
        let manifest = repo_sandbox_core::build::PlatformDigest {
            platform: Platform::LinuxArm64,
            digest: arm_digest.clone(),
        };
        let selected = platform_environment(&primary, &index, &manifest, false);
        assert_eq!(selected.image, index.image);
        assert_eq!(selected.digest, arm_digest);
        assert_ne!(selected.digest, primary.digest);
    }

    #[test]
    fn environment_images_are_owned_and_journaled_by_local_image_id() {
        let image = ImageRef::new(format!("sha256:{}", "d".repeat(64))).unwrap();
        let digest = format!("sha256:{}", "e".repeat(64));
        let candidate = owned_environment_image_candidate("task", "repository", &image, &digest);
        assert_eq!(candidate.identifier, image.to_string());
        assert_eq!(candidate.owner, digest);
        let options = owned_environment_options(BuildOptions::default(), "repository");
        assert_eq!(
            options.build_args.get("REPO_SANDBOX_REPOSITORY_ID"),
            Some(&"repository".to_owned())
        );
    }

    #[test]
    fn runner_uses_resolved_image_id_even_if_the_source_tag_is_later_repointed() {
        let manifest_digest =
            repo_sandbox_core::build::ImageDigest::new(format!("sha256:{}", "a".repeat(64)))
                .unwrap();
        let local_id = format!("sha256:{}", "b".repeat(64));
        let image = BuiltImage {
            image: ImageRef::new("repo-sandbox-task:mutable").unwrap(),
            digest: manifest_digest.clone(),
            platform_digests: Vec::new(),
        };
        let executor = InspectExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
            image_id: local_id.clone(),
        };
        let resolved = resolve_local_image_id_with(&executor, &image, &NeverCancelled).unwrap();
        assert_eq!(resolved.as_str(), local_id);
        // A later mutation of the source tag cannot change the ID already passed to RunSpec.
        let repointed_tag_id = format!("sha256:{}", "c".repeat(64));
        assert_ne!(resolved.as_str(), repointed_tag_id);
        let calls = executor.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].args.last().unwrap(), "repo-sandbox-task:mutable");
        assert_eq!(image.digest, manifest_digest);
        let candidate = owned_task_image_candidate("task", "repository", &resolved, "owner".into());
        assert_eq!(candidate.identifier, local_id);
        assert_ne!(candidate.identifier, image.digest.to_string());
    }

    #[test]
    fn cancelled_clean_skips_candidates_without_invoking_docker() {
        struct Cancelled;
        impl Cancellation for Cancelled {
            fn is_cancelled(&self) -> bool {
                true
            }
        }
        let candidate = CleanCandidate {
            task_id: "task".into(),
            repository_id: "repository".into(),
            kind: ResourceKind::Container,
            identifier: "must-not-be-inspected".into(),
            owner: "task".into(),
            state: ResourceState::Retained,
        };
        let result = execute_clean(
            &CleanPlan {
                candidates: vec![candidate],
                ..CleanPlan::default()
            },
            false,
            &Cancelled,
        )
        .unwrap();
        assert!(result.succeeded.is_empty());
        assert!(result.unfinished[0].contains("cancelled"));
        assert!(!result.complete());
    }

    #[test]
    fn clean_distinguishes_absent_objects_from_docker_failures() {
        assert!(docker_object_absent("Error: No such image: sha256:abc"));
        assert!(docker_object_absent("Error: No such container: task"));
        assert!(!docker_object_absent(
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock"
        ));
        assert!(!docker_object_absent("permission denied"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_workflow_state_root_is_rejected() {
        use std::os::unix::fs::symlink;
        let repository = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), repository.path().join(".repo-sandbox")).unwrap();
        let state = repository.path().join(".repo-sandbox");
        assert!(validate_state_root(repository.path(), &state).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_descendant_is_rejected_before_any_owned_write() {
        use std::os::unix::fs::symlink;
        for leaf in ["cache", "reports", "artifacts", "tasks"] {
            let repository = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let sentinel = outside.path().join("sentinel");
            fs::write(&sentinel, "unchanged").unwrap();
            let state = repository.path().join(".repo-sandbox");
            fs::create_dir(&state).unwrap();
            symlink(outside.path(), state.join(leaf)).unwrap();
            assert!(prepare_state_layout(repository.path(), &state).is_err());
            assert_eq!(fs::read_to_string(sentinel).unwrap(), "unchanged");
            assert_eq!(fs::read_dir(&state).unwrap().count(), 1);
        }
    }

    #[cfg(windows)]
    #[test]
    fn junction_state_descendant_is_rejected_before_any_owned_write() {
        for leaf in ["cache", "reports", "artifacts", "tasks"] {
            let repository = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let sentinel = outside.path().join("sentinel");
            fs::write(&sentinel, "unchanged").unwrap();
            let state = repository.path().join(".repo-sandbox");
            fs::create_dir(&state).unwrap();
            let junction = state.join(leaf);
            let status = std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    &junction.to_string_lossy(),
                    &outside.path().to_string_lossy(),
                ])
                .status()
                .unwrap();
            assert!(status.success());
            assert!(prepare_state_layout(repository.path(), &state).is_err());
            assert_eq!(fs::read_to_string(sentinel).unwrap(), "unchanged");
            assert_eq!(fs::read_dir(&state).unwrap().count(), 1);
            fs::remove_dir(&junction).unwrap();
        }
    }

    #[test]
    fn retained_source_trusts_only_the_owned_temp_parent_and_source_leaf() {
        let trusted = std::env::temp_dir()
            .join("repo-sandbox-source-abc")
            .join("source");
        assert!(trusted_source_path(&trusted.to_string_lossy()));
        assert!(!trusted_source_path(
            &std::env::temp_dir()
                .join("untrusted")
                .join("source")
                .to_string_lossy()
        ));
        assert!(!trusted_source_path(
            &std::env::temp_dir()
                .join("repo-sandbox-source-abc")
                .join("other")
                .to_string_lossy()
        ));
    }

    #[test]
    fn oci_reservation_rejects_existing_output_without_overwrite() {
        let temporary = tempfile::tempdir().unwrap();
        let output = temporary.path().join("layout");
        fs::create_dir(&output).unwrap();
        let error = OutputReservation::oci(&output).unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert!(output.is_dir());
    }

    #[test]
    fn output_reservation_never_creates_repository_local_bookkeeping() {
        let repository = tempfile::tempdir().unwrap();
        let parent = repository.path().join("uncreated");
        let output = parent.join("report.json");
        let reservation = OutputReservation::report(&output).unwrap();
        assert!(!parent.exists());
        assert_eq!(fs::read_dir(repository.path()).unwrap().count(), 0);
        drop(reservation);
    }

    #[test]
    fn output_reservation_is_recoverable_after_owner_process_termination() {
        let temporary = tempfile::tempdir().unwrap();
        let output = temporary.path().join("report.json");
        let ready = temporary.path().join("ready");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workflow::tests::output_reservation_process_helper",
                "--nocapture",
            ])
            .env("REPO_SANDBOX_RESERVATION_HELPER_OUTPUT", &output)
            .env("REPO_SANDBOX_RESERVATION_HELPER_READY", &ready)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ready.exists(), "reservation helper did not become ready");
        child.kill().unwrap();
        child.wait().unwrap();
        OutputReservation::report(&output).unwrap();
    }

    #[test]
    fn output_reservation_process_helper() {
        let (Some(output), Some(ready)) = (
            std::env::var_os("REPO_SANDBOX_RESERVATION_HELPER_OUTPUT"),
            std::env::var_os("REPO_SANDBOX_RESERVATION_HELPER_READY"),
        ) else {
            return;
        };
        let _reservation = OutputReservation::report(Path::new(&output)).unwrap();
        fs::write(ready, "ready").unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
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
        assert!(!registry_probe_authenticated(&output(
            Some(1),
            "credential helper executable not found"
        )));
        assert!(!registry_probe_authenticated(&output(
            Some(1),
            "unauthorized: manifest unknown"
        )));
    }

    #[test]
    fn single_platform_seed_uses_the_final_content_tag_not_staging() {
        let repository = RegistryRepository::new("registry.test/team/image").unwrap();
        let digest =
            repo_sandbox_core::build::ImageDigest::new(format!("sha256:{}", "a".repeat(64)))
                .unwrap();
        let target = registry_content_ref(&repository, &digest);
        assert_eq!(
            target.as_str(),
            format!("registry.test/team/image:sha256-{}", "a".repeat(64))
        );
        assert!(!target.as_str().contains("staging"));
    }

    fn default_execution_plan() -> ExecutionPlan {
        let config = repo_sandbox_core::config::Config::parse_yaml(
            "version: 1\ntemplate:\n  id: rust-bazel\n  parameters:\n    platform: linux/amd64\n",
        )
        .unwrap();
        let request = repo_sandbox_core::config::ExecutionRequest::resolve(
            &config,
            repo_sandbox_core::config::CliOverrides::default(),
        );
        let template = repo_sandbox_core::template::TemplateCatalog::builtin()
            .unwrap()
            .plan(&config.template, request.platform)
            .unwrap();
        ExecutionPlan::new(template, request)
    }

    #[test]
    fn default_outputs_do_not_report_a_false_none_collision() {
        let plan = default_execution_plan();
        assert!(plan.request.report.is_none());
        assert!(plan.request.oci_layout.is_none());
        validate_outputs(&plan).unwrap();
    }

    #[test]
    fn lexically_equivalent_output_paths_are_rejected_before_reservation() {
        let mut plan = default_execution_plan();
        plan.request.report = Some(PathBuf::from("reports/result.json"));
        plan.request.oci_layout = Some(PathBuf::from("reports/other/../result.json"));
        assert!(
            validate_outputs(&plan)
                .unwrap_err()
                .to_string()
                .contains("different")
        );
        assert_eq!(
            normalized_output_path(Path::new("reports/other/../result.json")).unwrap(),
            normalized_output_path(Path::new("reports/result.json")).unwrap()
        );
    }

    #[test]
    fn local_git_ref_is_rejected_instead_of_ignored() {
        let mut plan = default_execution_plan();
        plan.request.repository = Some(".".into());
        plan.request.git_ref = Some("main".into());
        assert!(
            validate_outputs(&plan)
                .unwrap_err()
                .to_string()
                .contains("only")
        );
    }

    #[test]
    fn explicit_image_phase_wins_over_git_text_in_diagnostics() {
        let environment =
            AppError::Environment("environment image: package command `git install` failed".into());
        let task = AppError::Environment("task image: git metadata failure".into());
        assert_eq!(failure_phase(&environment), "environment_image");
        assert_eq!(failure_phase(&task), "task_image");
    }

    #[test]
    fn bookkeeping_or_cleanup_failure_disqualifies_all_outputs() {
        use repo_sandbox_core::runner::CleanupResult;
        assert!(outputs_allowed(
            &RunStatus::Succeeded,
            CleanupResult::Removed,
            None
        ));
        assert!(!outputs_allowed(
            &RunStatus::Succeeded,
            CleanupResult::Failed,
            Some("journal failed")
        ));
        assert!(!outputs_allowed(
            &RunStatus::CommandFailed {
                phase: StepPhase::Build,
                step: "build".into(),
                exit_code: Some(1),
            },
            CleanupResult::Removed,
            None
        ));
    }

    #[test]
    fn trusted_catalog_is_embedded_and_not_loaded_from_the_target_repository() {
        let catalog = trusted_catalog().unwrap();
        let dockerfile = fs::read_to_string(
            catalog
                .path()
                .join("templates/rust-bazel/context/Dockerfile"),
        )
        .unwrap();
        assert_eq!(
            dockerfile,
            include_str!("../../../templates/rust-bazel/context/Dockerfile")
        );
        assert!(dockerfile.contains("io.repo-sandbox.owner"));
        assert!(dockerfile.contains("io.repo-sandbox.repository-id"));
    }

    #[test]
    fn remote_identity_is_credential_free_and_repository_specific() {
        let first = normalize_remote_identity("https://token@example.test/org/one.git").unwrap();
        let same = normalize_remote_identity("https://other@example.test/org/one.git/").unwrap();
        let second = normalize_remote_identity("https://example.test/org/two.git").unwrap();
        assert_eq!(first, same);
        assert_eq!(first, "https://example.test/org/one.git");
        assert_ne!(first, second);
        assert!(!first.contains("token"));
        assert!(!first.contains("secret"));
    }

    #[test]
    fn remote_urls_reject_query_and_fragment_data_before_network_access() {
        for repository in [
            "https://example.test/repo.git?token=sensitive-value",
            "https://example.test/repo.git#sensitive-fragment",
        ] {
            let error = validate_remote_repository(repository).unwrap_err();
            assert!(!error.to_string().contains("sensitive"));
        }
    }

    #[test]
    fn remote_authentication_uses_only_explicit_external_references() {
        let auth = RemoteAuthentication {
            https_username: Some("robot".into()),
            https_token_environment: Some("EXTERNAL_TOKEN".into()),
            ..RemoteAuthentication::default()
        };
        assert_eq!(
            external_git_authentication("https://example.test/repo.git", &auth).unwrap(),
            GitAuthentication::HttpsToken {
                username: "robot".into(),
                token: ExternalSecret::Environment("EXTERNAL_TOKEN".into()),
            }
        );
        let conflicting = RemoteAuthentication {
            https_token_environment: Some("TOKEN".into()),
            https_credential_helper: true,
            ..RemoteAuthentication::default()
        };
        assert!(
            external_git_authentication("https://example.test/repo.git", &conflicting).is_err()
        );
        let known_hosts_only = RemoteAuthentication {
            ssh_known_hosts: Some("known-hosts".into()),
            ..RemoteAuthentication::default()
        };
        assert!(
            external_git_authentication("person@example.test:org/repo.git", &known_hosts_only)
                .unwrap_err()
                .to_string()
                .contains("requires")
        );
        let agent = RemoteAuthentication {
            ssh_agent: true,
            ..RemoteAuthentication::default()
        };
        assert!(matches!(
            external_git_authentication("person@example.test:org/repo.git", &agent).unwrap(),
            GitAuthentication::SshAgent { .. }
        ));
    }

    #[test]
    fn scp_style_remote_classification_matches_snapshot_transport() {
        assert!(is_remote_repository("person@example.test:org/repo.git"));
        assert!(is_remote_repository("git@example.test:repo.git"));
        assert!(!is_remote_repository("C:\\work\\repo"));
        assert!(!is_remote_repository("relative:directory"));
    }

    #[test]
    fn secret_environment_values_must_be_exactly_representable() {
        assert!(validate_secret_value("TOKEN", b"value").is_ok());
        for value in [b"".as_slice(), b"value\n", b"value\r", b"a\0b"] {
            let error = validate_secret_value("TOKEN", value).unwrap_err();
            assert!(!error.to_string().contains("value"));
            assert!(error.to_string().contains("TOKEN"));
        }
    }

    #[test]
    fn clean_all_enumerates_local_and_trusted_remote_stores() {
        let repository = tempfile::tempdir().unwrap();
        let canonical = repository.path().canonicalize().unwrap();
        let local_id = repository_id(&canonical).unwrap();
        let remote_id = format!("sha256:{}", "b".repeat(64));
        let state = canonical.join(".repo-sandbox");
        let local_tasks = state.join("tasks");
        let remote_state = state.join("remotes").join("b".repeat(64));
        let remote_tasks = remote_state.join("tasks");
        fs::create_dir_all(&local_tasks).unwrap();
        fs::create_dir_all(&remote_tasks).unwrap();
        let candidate = |repository_id: String, identifier: PathBuf| CleanCandidate {
            task_id: "task".into(),
            repository_id: repository_id.clone(),
            kind: ResourceKind::Cache,
            identifier: identifier.display().to_string(),
            owner: repository_id,
            state: ResourceState::Registered,
        };
        fs::write(
            local_tasks.join("local.json"),
            serde_json::to_vec(&[candidate(local_id, state.join("cache"))]).unwrap(),
        )
        .unwrap();
        fs::write(
            remote_tasks.join("remote.json"),
            serde_json::to_vec(&[candidate(remote_id, remote_state.join("cache"))]).unwrap(),
        )
        .unwrap();
        let plan = SystemWorkflow
            .plan(&CleanRequest {
                repository: canonical,
                all: true,
                include_images: false,
                include_cache: false,
                dry_run: true,
            })
            .unwrap();
        assert_eq!(plan.candidates.len(), 2);
        assert_eq!(plan.journal_roots.len(), 2);
        assert!(plan.refused.is_empty());
    }

    #[test]
    fn clean_continues_after_cleanup_state_recording_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let repository_id = format!("sha256:{}", "c".repeat(64));
        let make = |name: &str| {
            let path = temporary.path().join(name);
            fs::create_dir(&path).unwrap();
            fs::write(path.join(OWNER_MARKER), &repository_id).unwrap();
            CleanCandidate {
                task_id: name.into(),
                repository_id: repository_id.clone(),
                kind: ResourceKind::Cache,
                identifier: path.display().to_string(),
                owner: repository_id.clone(),
                state: ResourceState::Registered,
            }
        };
        let first = make("first");
        let second = make("second");
        let not_a_directory = temporary.path().join("journal-file");
        fs::write(&not_a_directory, "not a directory").unwrap();
        let plan = CleanPlan {
            candidates: vec![first.clone(), second.clone()],
            refused: Vec::new(),
            manifest_root: None,
            journal_roots: std::collections::BTreeMap::from([(repository_id, not_a_directory)]),
            lease_path: None,
        };
        let result = CleanPort::execute(&SystemWorkflow, &plan, false).unwrap();
        assert_eq!(result.succeeded.len(), 2);
        assert_eq!(result.failed.len(), 2);
        assert!(!Path::new(&first.identifier).exists());
        assert!(!Path::new(&second.identifier).exists());
    }

    #[test]
    fn registration_after_cleanup_supersedes_the_cleaned_state() {
        let repository = tempfile::tempdir().unwrap();
        let canonical = repository.path().canonicalize().unwrap();
        let repository_id = repository_id(&canonical).unwrap();
        let state = canonical.join(".repo-sandbox");
        let cache = state.join("cache");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join(OWNER_MARKER), &repository_id).unwrap();
        let candidate = CleanCandidate {
            task_id: "original".into(),
            repository_id: repository_id.clone(),
            kind: ResourceKind::Cache,
            identifier: cache.display().to_string(),
            owner: repository_id,
            state: ResourceState::Registered,
        };
        let first = ManifestJournal::create(&state, "original").unwrap();
        first.append(std::slice::from_ref(&candidate)).unwrap();
        append_cleanup_state(&state.join("tasks"), &candidate).unwrap();
        let mut rebuilt = candidate.clone();
        rebuilt.task_id = "rebuilt".into();
        ManifestJournal::create(&state, "rebuilt")
            .unwrap()
            .append(std::slice::from_ref(&rebuilt))
            .unwrap();

        let plan = SystemWorkflow
            .plan(&CleanRequest {
                repository: canonical,
                include_cache: true,
                dry_run: true,
                ..CleanRequest::default()
            })
            .unwrap();
        assert_eq!(plan.candidates, vec![rebuilt]);
    }

    #[test]
    fn journal_sequence_is_monotonic_across_reopen_and_concurrent_processes() {
        let root = tempfile::tempdir().unwrap();
        let helper = std::env::current_exe().unwrap();
        let test_name = "workflow::tests::journal_sequence_process_helper";
        let spawn = || {
            std::process::Command::new(&helper)
                .args(["--exact", test_name, "--nocapture"])
                .env("REPO_SANDBOX_SEQUENCE_HELPER_ROOT", root.path())
                .spawn()
                .unwrap()
        };
        let mut first = spawn();
        let mut second = spawn();
        assert!(first.wait().unwrap().success());
        assert!(second.wait().unwrap().success());
        let mut sequences = fs::read_dir(root.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| new_event_sequence(&entry.path()))
            .collect::<Vec<_>>();
        sequences.sort_unstable();
        assert_eq!(sequences, (1..=40).collect::<Vec<_>>());
        assert_eq!(next_journal_sequence(root.path()).unwrap(), 41);
    }

    #[test]
    fn journal_sequence_process_helper() {
        let Some(root) = std::env::var_os("REPO_SANDBOX_SEQUENCE_HELPER_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        for _ in 0..20 {
            let sequence = next_journal_sequence(&root).unwrap();
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join(format!(
                    "event-{sequence:020}-helper-{}.json",
                    std::process::id()
                )))
                .unwrap();
        }
    }

    #[test]
    fn active_workflow_lease_blocks_only_its_repository_cleanup() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first_state = first.path().join(".repo-sandbox");
        let second_state = second.path().join(".repo-sandbox");
        let _active = WorkflowLease::shared(&first_state).unwrap();
        let candidate = |_root: &Path, name: &str| CleanCandidate {
            task_id: name.into(),
            repository_id: format!("sha256:{}", name.repeat(64)),
            kind: ResourceKind::Container,
            identifier: name.into(),
            owner: name.into(),
            state: ResourceState::Registered,
        };
        let blocked = CleanPlan {
            candidates: vec![candidate(first.path(), "a")],
            lease_path: Some(first_state.join(".workflow.lock")),
            ..CleanPlan::default()
        };
        let independent = CleanPlan {
            candidates: vec![candidate(second.path(), "b")],
            lease_path: Some(second_state.join(".workflow.lock")),
            ..CleanPlan::default()
        };
        let blocked = CleanPort::execute(&SystemWorkflow, &blocked, false).unwrap();
        assert!(blocked.unfinished[0].contains("active workflow"));
        assert!(!blocked.complete());
        let independent = CleanPort::execute(&SystemWorkflow, &independent, true).unwrap();
        assert!(independent.skipped[0].contains("dry-run"));
        assert!(!second_state.exists());
    }

    #[test]
    fn failed_container_cleanup_remains_registered_for_retry() {
        use repo_sandbox_core::runner::CleanupResult;
        assert_eq!(
            container_resource_state(CleanupResult::Failed),
            ResourceState::Registered
        );
        assert_eq!(
            container_resource_state(CleanupResult::Removed),
            ResourceState::Cleaned
        );
        assert_eq!(
            container_resource_state(CleanupResult::RetainedOnFailure),
            ResourceState::Retained
        );
    }
}
