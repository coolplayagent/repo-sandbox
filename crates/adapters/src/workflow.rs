//! Concrete composition root for the build/verify/clean application use cases.

use crate::artifacts::{OWNER_MARKER, cleanup_owned_temp_source};
use crate::buildkit::{
    BuildKit, BuildOptions, BuildRequest, CacheConfig, Cancellation, ImageOutput, ProcessExecutor,
    ProcessInvocation, Progress, SystemProcessExecutor,
};
use crate::cancellation::{DeadlineCancellation, ProcessCancellation};
use crate::docker_runner::{DockerExecutor, DockerRunner, SystemClock, SystemDockerExecutor};
use crate::doctor::{DoctorProbe, SystemDoctorProbe};
use crate::registry::{DockerRegistry, SystemRegistryExecutor};
use crate::snapshot::GitSnapshotter;
use crate::task_image::{TaskImageBuilder, TaskImageOptions, TaskImageRequest};
use repo_sandbox_core::AppError;
use repo_sandbox_core::application::{
    CleanCandidate, CleanPlan, CleanPort, CleanRequest, CleanResult, ExecutionPlan, ResourceKind,
    ResourceState, WorkflowFailureReport, WorkflowFailureStatus, WorkflowMode, WorkflowPort,
    WorkflowResult, configuration_source_digest, write_failure_report,
};
use repo_sandbox_core::build::{BuiltImage, ImageRef};
use repo_sandbox_core::config::{Platform, RemoteAuthentication};
use repo_sandbox_core::registry::{
    PublicationFactKind, PublicationFinality, PublishRequest, PublishedImage, RegistryRepository,
    RegistryTag, RemotePublicationFact,
};
use repo_sandbox_core::runner::{
    ConfigSummary, RunResources, RunSpec, RunStatus, SecretMount, StepPhase, write_report_json,
};
use repo_sandbox_core::snapshot::{
    CleanupPolicy, ExternalSecret, GitAuthentication, SnapshotOptions, SnapshotOrigin, SourceSpec,
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
        let report_path = normalized_output_path(
            &plan
                .request
                .report
                .clone()
                .unwrap_or_else(|| state.join("reports").join(format!("{task_id}.json"))),
        )?;
        validate_output_path_overlap(plan)?;
        validate_state_outputs(&state, &report_path, plan.request.oci_layout.as_deref())?;
        // Finish every pure plan validation before creating an explicit report
        // parent or any reservation/state file.
        validate_outputs(plan)?;
        validate_required_secret_environment(plan)?;
        let oci_destination = plan
            .request
            .oci_layout
            .as_deref()
            .map(ExternalOciGuard::prepare)
            .transpose()?;
        let report_destination =
            ReportDestination::prepare(classify_report_destination(&state, &report_path)?)?;
        let _report_reservation = OutputReservation::report(&report_path)?;
        let mut completed_report = None;
        let mut early_publication_progress = Vec::new();
        let mut bound_state = None;
        let result = (|| {
            let (state_guard, _workflow_lease, journal) =
                prepare_leased_workflow_state(&repository, &state, &task_id)?;
            bound_state = Some(state_guard.clone());
            let registry = plan.template.execution.registry.as_ref();
            preflight(plan, &repository, &task_id, &cancellation, |fact| {
                early_publication_progress.push(fact);
            })?;
            let cache = state.join("cache");
            let cache_io = state_guard.bound_path(&cache)?;
            write_state_file(&cache_io.join(OWNER_MARKER), repository_id.as_bytes())?;
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
            preserve_requested_ref(&mut materialized.snapshot.origin, &plan.request);
            verify_materialized_configuration(plan, materialized.path())?;
            let trusted_catalog = trusted_catalog()?;
            let catalog_root = trusted_catalog.path().to_path_buf();
            let cache_import = cache_io.join("environment");
            let cache_export = task_cache_export(&cache_io, &task_id);
            if cache_export.exists() {
                fs::remove_dir_all(&cache_export)
                    .map_err(environment("remove stale owned cache export"))?;
            }
            let cache_import_lease = CacheLease::shared(&cache_io, &cancellation)?;
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
            let environment_result = BuildKit::new(SystemProcessExecutor)
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
                .map_err(|error| bounded_error("environment image", error, &cancellation));
            let environment_image = match environment_result {
                Ok(image) => image,
                Err(primary) => {
                    return Err(clean_failed_cache_export(&cache_export, primary));
                }
            };
            drop(cache_import_lease);
            if cache_export.exists() {
                rotate_cache_export(&cache_io, &cache_export, &cache_import, &cancellation)?;
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
            if let Err(primary) = journal.append(&[owned_task_image_candidate(
                &task_id,
                &repository_id,
                &execution_image,
                task_image.identity.oci_value(),
            )]) {
                return Err(registration_failure_with_safe_retention(
                    primary,
                    &execution_image,
                ));
            }

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
                let value = validated_secret_text(name, &value)?;
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
            let artifact_root = state_guard
                .bound_path(&state.join("artifacts"))?
                .join(&task_id);
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
            report
                .publication_progress
                .extend(early_publication_progress.clone());

            let mut bookkeeping_errors = Vec::new();
            if report.cleanup == repo_sandbox_core::runner::CleanupResult::RetainedOnFailure
                && materialized.is_automatically_cleaned()
            {
                let registration =
                    retain_source_after_registration(&mut materialized, |source_path| {
                        fs::write(source_path.join(OWNER_MARKER), &task_id)
                            .map_err(environment("mark retained source"))
                            .and_then(|()| {
                                journal.append(&[CleanCandidate {
                                    task_id: task_id.clone(),
                                    repository_id: repository_id.clone(),
                                    kind: ResourceKind::Source,
                                    identifier: source_path.display().to_string(),
                                    owner: task_id.clone(),
                                    state: ResourceState::Retained,
                                }])
                            })
                    });
                if let Err(error) = registration {
                    bookkeeping_errors.push(error.to_string());
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

            if output_eligible && let Some(destination) = &oci_destination {
                let output = destination.bound_path()?;
                export_verified_oci(
                    plan,
                    &catalog_root,
                    &materialized,
                    &environment_image,
                    &environment_layout,
                    &task_image.image,
                    &configuration_digest,
                    &repository_id,
                    &output,
                    &cancellation,
                )?;
            }

            let mut publication_cleanup_error = None;
            let published = if plan.request.push && output_eligible {
                let policy = registry.expect("checked registry");
                let repository =
                    RegistryRepository::new(&policy.repository).map_err(AppError::Configuration)?;
                let aliases = policy
                    .aliases
                    .iter()
                    .map(|value| RegistryTag::new(value).map_err(AppError::Configuration))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut local_seed = None;
                let published_task = if plan.request.platforms.len() > 1 {
                    build_multi_platform_task(
                        plan,
                        &catalog_root,
                        &materialized,
                        &environment_image,
                        &environment_layout,
                        &task_image.image,
                        &configuration_digest,
                        &repository_id,
                        &repository,
                        &cancellation,
                        |progress| {
                            report.publication_progress.push(progress);
                            completed_report = Some(report.clone());
                        },
                    )?
                } else {
                    let seeded = seed_registry(
                        &task_image.image.image,
                        &repository,
                        &task_image.image.digest,
                        &cancellation,
                    )?;
                    let reference = seeded.reference.clone();
                    local_seed = Some(seeded);
                    (reference, task_image.image.clone())
                };
                if plan.request.platforms.len() == 1 {
                    // The seed push is already an irreversible remote fact. Record it
                    // before alias publication/verification so a later failure report
                    // never denies that the immutable content tag exists.
                    report.published = Some(seeded_publication(&published_task));
                    completed_report = Some(report.clone());
                }
                let publish_request = PublishRequest {
                    source: published_task.0,
                    repository,
                    digest: published_task.1.digest,
                    platform_digests: published_task.1.platform_digests,
                    aliases,
                };
                let publication_result = DockerRegistry::new(SystemRegistryExecutor)
                    .publish_with_progress(&publish_request, &cancellation, |published| {
                        report.published = Some(published.clone());
                        completed_report = Some(report.clone());
                    })
                    .map_err(|error| bounded_error("publish", error, &cancellation));
                let publication = match publication_result {
                    Ok(publication) => publication,
                    Err(primary) => {
                        let seed = local_seed
                            .as_ref()
                            .map(|seed| (&seed.reference, seed.owned_local_tag));
                        let error =
                            cleanup_seed_after_publication_failure(&mut report, seed, primary);
                        completed_report = Some(report.clone());
                        return Err(error);
                    }
                };
                // Publication is an irreversible remote fact. Persist it in the
                // in-memory failure-report snapshot before attempting local tag
                // cleanup, which is a separate best-effort cleanup phase.
                report.published = Some(publication.clone());
                completed_report = Some(report.clone());
                if let Some(seed) = local_seed
                    && seed.owned_local_tag
                {
                    publication_cleanup_error = apply_publication_cleanup(
                        &mut report,
                        remove_local_registry_tag(&seed.reference, &cancellation),
                    );
                }
                Some(publication)
            } else {
                None
            };
            report.published = published.clone();
            annotate_report(&mut report);
            completed_report = Some(report.clone());

            let report_io = report_destination.bound_path(bound_state.as_ref())?;
            write_report_json(&report, &report_io).map_err(environment("write atomic report"))?;
            let result = WorkflowResult {
                plan_digest: plan.digest.clone(),
                report: report.clone(),
                published,
            };
            if let Some(error) = publication_cleanup_error {
                return Err(error);
            }
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
            && !report_destination.exists(bound_state.as_ref())?
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
                if let Some(state) = &bound_state {
                    state.ensure()?;
                }
                let Some(report_io) =
                    optional_failure_report_path(&bound_state, &report_destination)?
                else {
                    return result;
                };
                write_report_json(&report, &report_io).map_err(|write| {
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
                publication_progress: early_publication_progress.clone(),
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
            if let Some(state) = &bound_state {
                state.ensure()?;
            }
            let Some(report_io) = optional_failure_report_path(&bound_state, &report_destination)?
            else {
                return result;
            };
            write_failure_report(&failure, &report_io).map_err(|write| {
                AppError::Environment(format!("write failure report: {write}; primary: {error}"))
            })?;
        }
        result
    }
}

fn retain_source_after_registration(
    materialized: &mut crate::snapshot::MaterializedSnapshot,
    register: impl FnOnce(&Path) -> Result<(), AppError>,
) -> Result<(), AppError> {
    register(materialized.path())?;
    // Relinquish automatic deletion only after ownership registration is
    // durable. An error leaves TempDir deletion armed.
    materialized.retain_on_failure();
    Ok(())
}

fn optional_failure_report_path(
    bound_state: &Option<StateLayoutGuard>,
    destination: &ReportDestination,
) -> Result<Option<PathBuf>, AppError> {
    if bound_state.is_none() && matches!(destination, ReportDestination::State { .. }) {
        return Ok(None);
    }
    destination.bound_path(bound_state.as_ref()).map(Some)
}

fn verify_materialized_configuration(plan: &ExecutionPlan, root: &Path) -> Result<(), AppError> {
    let Some(expected) = plan.request.repository_config_digest.as_deref() else {
        return Ok(());
    };
    let source = fs::read(root.join(".repo-sandbox.yaml"))
        .map_err(environment("read materialized repository configuration"))?;
    if configuration_source_digest(&source) != expected {
        return Err(AppError::Configuration(
            "materialized .repo-sandbox.yaml differs from the configuration used to create the execution plan".into(),
        ));
    }
    Ok(())
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
            plan.journal_revisions
                .insert(manifests.clone(), journal_revision(&entries));
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
        execute_clean(plan, dry_run, &ProcessCancellation)
    }
}

fn execute_clean(
    plan: &CleanPlan,
    dry_run: bool,
    cancellation: &dyn Cancellation,
) -> Result<CleanResult, AppError> {
    execute_clean_with_hook(plan, dry_run, cancellation, || {})
}

fn execute_clean_with_hook(
    plan: &CleanPlan,
    dry_run: bool,
    cancellation: &dyn Cancellation,
    after_revision: impl FnOnce(),
) -> Result<CleanResult, AppError> {
    let mut result = CleanResult {
        dry_run,
        skipped: plan.refused.clone(),
        ..CleanResult::default()
    };
    let bound_journals = if dry_run {
        None
    } else {
        bind_clean_journals(plan)?
    };
    let _lease = if dry_run {
        None
    } else if let Some(path) = &plan.lease_path {
        let lease_path = bound_journals
            .as_ref()
            .map(|bound| bound.lease.as_path())
            .unwrap_or(path);
        match WorkflowLease::exclusive(lease_path)? {
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
    if let Some(bound) = &bound_journals {
        bound.guard.ensure()?;
    }
    if !dry_run {
        for (root, expected) in &plan.journal_revisions {
            let root = bound_journals
                .as_ref()
                .and_then(|bound| bound.roots.get(root))
                .unwrap_or(root);
            let mut entries = fs::read_dir(root)
                .map_err(environment("revalidate task manifests"))?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(environment("revalidate task manifest entry"))?;
            entries.sort_by_key(|path| journal_event_order(path));
            if journal_revision(&entries) != *expected {
                result
                    .unfinished
                    .push("clean plan changed while awaiting confirmation; rerun clean".into());
                return Ok(result);
            }
        }
    }
    after_revision();
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
                record_cleaned_state(
                    plan,
                    bound_journals.as_ref(),
                    candidate,
                    "removed",
                    &mut result,
                );
            }
            Ok(RemovalOutcome::Absent) => {
                result.absent.push(candidate.identifier.clone());
                record_cleaned_state(
                    plan,
                    bound_journals.as_ref(),
                    candidate,
                    "absent",
                    &mut result,
                );
            }
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

fn record_cleaned_state(
    plan: &CleanPlan,
    bound_journals: Option<&CleanBoundJournals>,
    candidate: &CleanCandidate,
    disposition: &str,
    result: &mut CleanResult,
) {
    let root = plan
        .journal_roots
        .get(&candidate.repository_id)
        .or(plan.manifest_root.as_ref());
    if let Some(root) = root {
        let append = (|| {
            let root = if let Some(bound) = bound_journals {
                bound.guard.ensure()?;
                bound.roots.get(root).ok_or_else(|| {
                    AppError::Environment("cleanup journal root is not bound".into())
                })?
            } else {
                root
            };
            append_cleanup_state(root, candidate)
        })();
        if let Err(error) = append {
            result.failed.push(format!(
                "{}: {disposition} but failed to record cleanup state: {error}",
                candidate.identifier
            ));
        }
    }
}

struct CleanBoundJournals {
    guard: StateLayoutGuard,
    lease: PathBuf,
    roots: std::collections::BTreeMap<PathBuf, PathBuf>,
}

fn bind_clean_journals(plan: &CleanPlan) -> Result<Option<CleanBoundJournals>, AppError> {
    let Some(lease) = &plan.lease_path else {
        return Ok(None);
    };
    if plan.journal_roots.is_empty() {
        return Ok(None);
    }
    let state = lease.parent().ok_or_else(|| {
        AppError::Environment("cleanup lease has no workflow state parent".into())
    })?;
    let repository = state
        .parent()
        .ok_or_else(|| AppError::Environment("workflow state has no repository parent".into()))?;
    let mut components = vec![state.to_path_buf()];
    for root in plan.journal_roots.values() {
        let parent = root
            .parent()
            .ok_or_else(|| AppError::Environment("cleanup journal has no state parent".into()))?;
        components.push(parent.to_path_buf());
        components.push(root.clone());
    }
    components.sort();
    components.dedup();
    let guard = StateLayoutGuard::capture(repository, &components)?;
    let roots = plan
        .journal_roots
        .values()
        .map(|root| Ok((root.clone(), guard.bound_path(root)?)))
        .collect::<Result<_, AppError>>()?;
    let lease = guard.bound_path(lease)?;
    Ok(Some(CleanBoundJournals {
        guard,
        lease,
        roots,
    }))
}

fn journal_revision(entries: &[PathBuf]) -> Vec<String> {
    entries
        .iter()
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .filter_map(|path| {
            path.file_name()
                .map(|value| value.to_string_lossy().into_owned())
        })
        .collect()
}

#[derive(Debug)]
struct WorkflowLease {
    _file: fs::File,
}

impl WorkflowLease {
    fn open(path: &Path) -> Result<fs::File, AppError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(environment("create workflow lease directory"))?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            #[cfg(any(target_os = "linux", target_os = "android"))]
            const O_NOFOLLOW: i32 = 0x0002_0000;
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            const O_NOFOLLOW: i32 = 0x0000_0100;
            options.custom_flags(O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let file = options
            .open(path)
            .map_err(environment("open workflow lease"))?;
        let metadata = file
            .metadata()
            .map_err(environment("inspect workflow lease"))?;
        if is_link_or_reparse(&metadata)
            || !metadata.is_file()
            || !state_file_has_single_link(&file, &metadata)
        {
            return Err(AppError::Environment(
                "workflow lease must be an owned regular single-link file".into(),
            ));
        }
        Ok(file)
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

fn preserve_requested_ref(
    origin: &mut SnapshotOrigin,
    request: &repo_sandbox_core::config::ExecutionRequest,
) {
    if let SnapshotOrigin::RemoteGit { requested_ref, .. } = origin {
        *requested_ref = request
            .requested_git_ref
            .clone()
            .unwrap_or_else(|| "HEAD".into());
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
    validate_output_path_overlap(plan)
}

fn validate_output_path_overlap(plan: &ExecutionPlan) -> Result<(), AppError> {
    if let (Some(oci), Some(report)) = (&plan.request.oci_layout, &plan.request.report) {
        let oci = resolved_future_path(oci)?;
        let report = resolved_future_path(report)?;
        if oci == report || oci.starts_with(&report) || report.starts_with(&oci) {
            return Err(AppError::Configuration(
                "--oci-layout and --report-path must not overlap".into(),
            ));
        }
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

fn validate_state_outputs(state: &Path, report: &Path, oci: Option<&Path>) -> Result<(), AppError> {
    let state = resolved_future_path(state)?;
    let report = resolved_future_path(report)?;
    if let Some(oci) = oci {
        let oci = resolved_future_path(oci)?;
        if oci == state || oci.starts_with(&state) || state.starts_with(&oci) {
            return Err(AppError::Configuration(
                "--oci-layout must not overlap workflow state".into(),
            ));
        }
    }
    if report == state || state.starts_with(&report) {
        return Err(AppError::Configuration(
            "--report-path must not overlap workflow state".into(),
        ));
    }
    if report.starts_with(&state) {
        let reports = state.join("reports");
        if report == reports || !report.starts_with(&reports) {
            return Err(AppError::Configuration(
                "state-local --report-path must be a file beneath .repo-sandbox/reports".into(),
            ));
        }
    }
    Ok(())
}

/// Resolve the identity of the deepest existing ancestor and then append the
/// still-missing lexical suffix. This detects aliases through symlinks and
/// junctions without requiring the requested output itself to exist.
fn resolved_future_path(path: &Path) -> Result<PathBuf, AppError> {
    let path = normalized_output_path(path)?;
    let mut ancestor = path.as_path();
    let mut suffix = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let mut resolved = ancestor
                    .canonicalize()
                    .map_err(environment("resolve output ancestor"))?;
                for component in suffix.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = ancestor.file_name().ok_or_else(|| {
                    AppError::Configuration(format!(
                        "output path has no existing ancestor: {}",
                        path.display()
                    ))
                })?;
                suffix.push(name.to_os_string());
                ancestor = ancestor.parent().ok_or_else(|| {
                    AppError::Configuration(format!(
                        "output path escapes its filesystem root: {}",
                        path.display()
                    ))
                })?;
            }
            Err(error) => return Err(environment("inspect output ancestor")(error)),
        }
    }
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
        RunStatus::InfrastructureFailed { operation, message } => (
            infrastructure_report_phase(operation),
            3,
            format!("{operation}: {message}"),
        ),
    };
    report.phase = phase.into();
    report.exit_code = exit_code;
    report.message = message;
}

fn infrastructure_report_phase(operation: &str) -> &'static str {
    match operation {
        "create owned container" | "start owned container" => "environment",
        _ => "runner",
    }
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

fn validate_required_secret_environment(plan: &ExecutionPlan) -> Result<(), AppError> {
    for name in &plan.template.execution.secret_environment {
        let value = std::env::var_os(name).ok_or_else(|| {
            AppError::Configuration(format!("required secret environment `{name}` is not set"))
        })?;
        validated_secret_text(name, &value)?;
    }
    Ok(())
}

fn validated_secret_text<'a>(name: &str, value: &'a std::ffi::OsStr) -> Result<&'a str, AppError> {
    let value = value.to_str().ok_or_else(|| {
        AppError::Configuration(format!(
            "required secret environment `{name}` is not valid UTF-8"
        ))
    })?;
    validate_secret_value(name, value.as_bytes())?;
    Ok(value)
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
    primary_environment_layout: &Path,
    verified_task: &BuiltImage,
    configuration_digest: &ConfigurationDigest,
    repository_id: &str,
    repository: &RegistryRepository,
    cancellation: &DeadlineCancellation,
    mut on_progress: impl FnMut(RemotePublicationFact),
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
    on_progress(RemotePublicationFact {
        kind: PublicationFactKind::EnvironmentStaging,
        reference: environment.image.clone(),
        digest: environment.digest.clone(),
        verified: true,
        finality: PublicationFinality::Staging,
    });
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
                    environment_oci_layout: (*platform == plan.request.platform)
                        .then_some(primary_environment_layout),
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
        // Before a final task index exists, retain this explicitly typed
        // staging fact so failures never claim the push had no side effects.
        on_progress(RemotePublicationFact {
            kind: PublicationFactKind::TaskStaging,
            reference: task.image.image.clone(),
            digest: task.image.digest.clone(),
            verified: true,
            finality: PublicationFinality::Staging,
        });
        sources.push(format!("{}@{}", task.image.image, task.image.digest));
        task_manifests.push(repo_sandbox_core::build::PlatformDigest {
            platform: *platform,
            digest: task.image.digest,
        });
    }
    let index = multi_platform_index_ref(
        repository,
        &plan.digest,
        materialized.snapshot.id.as_str(),
        &sources,
    )?;
    let digest = create_multi_platform_index(&index, &sources, cancellation)?;
    on_progress(RemotePublicationFact {
        kind: PublicationFactKind::TaskIndexStaging,
        reference: index.clone(),
        digest: digest.clone(),
        verified: true,
        finality: PublicationFinality::Staging,
    });
    Ok((
        index.clone(),
        BuiltImage {
            image: index,
            digest,
            platform_digests: task_manifests,
        },
    ))
}

fn multi_platform_index_ref(
    repository: &RegistryRepository,
    plan_digest: &str,
    source_digest: &str,
    sources: &[String],
) -> Result<ImageRef, AppError> {
    let mut source_hasher = Sha256::new();
    for source in sources {
        source_hasher.update(source.len().to_le_bytes());
        source_hasher.update(source.as_bytes());
    }
    let manifest_identity = format!("{:x}", source_hasher.finalize());
    ImageRef::new(format!(
        "{}:multi-{}-{}-{}",
        repository.as_str(),
        short_digest(plan_digest),
        short_digest(source_digest),
        &manifest_identity[..12]
    ))
    .map_err(AppError::Configuration)
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
    merge_oci_layouts(&layouts, output, temporary.path(), cancellation)
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
    cancellation: &dyn Cancellation,
) -> Result<(), AppError> {
    merge_oci_layouts_with_hook(layouts, output, temporary_root, cancellation, || {})
}

fn merge_oci_layouts_with_hook(
    layouts: &[(Platform, PathBuf)],
    output: &Path,
    temporary_root: &Path,
    cancellation: &dyn Cancellation,
    before_commit: impl FnOnce(),
) -> Result<(), AppError> {
    ensure_oci_not_cancelled(cancellation)?;
    let merged = temporary_root.join("merged");
    fs::create_dir(&merged).map_err(environment("create merged OCI layout"))?;
    let blobs = merged.join("blobs");
    fs::create_dir(&blobs).map_err(environment("create merged OCI blobs"))?;
    let mut manifests = Vec::new();
    for (platform, layout) in layouts {
        ensure_oci_not_cancelled(cancellation)?;
        copy_tree(&layout.join("blobs"), &blobs, cancellation)?;
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
    before_commit();
    // This is the final cancellation boundary: after the no-replace rename,
    // the requested OCI artifact is a committed external fact.
    ensure_oci_not_cancelled(cancellation)?;
    rename_directory_no_replace(&merged, output).map_err(environment(
        "publish OCI layout atomically without replacement",
    ))
}

#[cfg(target_os = "linux")]
fn rename_directory_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    const AT_FDCWD: i32 = -100;
    const RENAME_NOREPLACE: u32 = 1;
    unsafe extern "C" {
        fn renameat2(
            olddirfd: i32,
            oldpath: *const std::ffi::c_char,
            newdirfd: i32,
            newpath: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in OCI source"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in OCI destination")
    })?;
    // SAFETY: both C strings remain alive for the syscall and file descriptors
    // use the documented AT_FDCWD sentinel.
    if unsafe {
        renameat2(
            AT_FDCWD,
            source.as_ptr(),
            AT_FDCWD,
            destination.as_ptr(),
            RENAME_NOREPLACE,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn rename_directory_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    const RENAME_EXCL: u32 = 0x4;
    unsafe extern "C" {
        fn renamex_np(
            old: *const std::ffi::c_char,
            new: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in OCI source"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in OCI destination")
    })?;
    // SAFETY: both C strings remain alive for the libc call.
    if unsafe { renamex_np(source.as_ptr(), destination.as_ptr(), RENAME_EXCL) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_directory_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    if destination.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "OCI destination already exists",
        ));
    }
    fs::rename(source, destination)
}

fn ensure_oci_not_cancelled(cancellation: &dyn Cancellation) -> Result<(), AppError> {
    if cancellation.is_cancelled() {
        Err(AppError::Environment(
            "OCI publication cancelled or timed out".into(),
        ))
    } else {
        Ok(())
    }
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    cancellation: &dyn Cancellation,
) -> Result<(), AppError> {
    for entry in fs::read_dir(source).map_err(environment("read OCI blobs"))? {
        ensure_oci_not_cancelled(cancellation)?;
        let entry = entry.map_err(environment("read OCI blob entry"))?;
        let target = destination.join(entry.file_name());
        if entry
            .file_type()
            .map_err(environment("inspect OCI blob entry"))?
            .is_dir()
        {
            fs::create_dir_all(&target).map_err(environment("create OCI blob directory"))?;
            copy_tree(&entry.path(), &target, cancellation)?;
        } else if !target.exists() {
            let mut input = fs::File::open(entry.path()).map_err(environment("open OCI blob"))?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)
                .map_err(environment("create OCI blob"))?;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                ensure_oci_not_cancelled(cancellation)?;
                let count = input
                    .read(&mut buffer)
                    .map_err(environment("read OCI blob"))?;
                if count == 0 {
                    break;
                }
                output
                    .write_all(&buffer[..count])
                    .map_err(environment("write OCI blob"))?;
            }
            output.sync_all().map_err(environment("sync OCI blob"))?;
        }
    }
    Ok(())
}

fn preflight(
    plan: &ExecutionPlan,
    repository: &Path,
    task_id: &str,
    cancellation: &DeadlineCancellation,
    on_publication: impl FnMut(RemotePublicationFact),
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
    let free = workflow_available_space(repository, cancellation)?;
    let required = (1024_u64 * 1024 * 1024)
        .max(u64::from(plan.template.execution.resources.temporary_storage_mb) * 2 * 1024 * 1024);
    if free < required {
        return Err(AppError::Environment(format!(
            "disk preflight requires {required} bytes free, found {free} bytes"
        )));
    }
    preflight_writable_layer_quota(
        &SystemProcessExecutor,
        plan.template.execution.resources.temporary_storage_mb,
        cancellation,
    )?;
    if plan.request.push {
        let policy = plan
            .template
            .execution
            .registry
            .as_ref()
            .expect("validated before preflight");
        preflight_registry_with(
            &SystemProcessExecutor,
            &policy.repository,
            task_id,
            plan.request.platforms.len() > 1,
            cancellation,
            on_publication,
        )?;
    }
    Ok(())
}

fn preflight_registry_with(
    executor: &impl ProcessExecutor,
    repository: &str,
    task_id: &str,
    buildx_boundary: bool,
    cancellation: &DeadlineCancellation,
    mut on_publication: impl FnMut(RemotePublicationFact),
) -> Result<(), AppError> {
    let repository = RegistryRepository::new(repository).map_err(AppError::Configuration)?;
    let tag = RegistryTag::new(format!("preflight-{task_id}")).map_err(AppError::Configuration)?;
    let probe = repository.tagged(&tag);
    let temporary = tempfile::Builder::new()
        .prefix("repo-sandbox-registry-preflight-")
        .tempdir()
        .map_err(environment("create registry preflight context"))?;
    fs::write(
        temporary.path().join("Dockerfile"),
        format!(
            "FROM scratch\nLABEL io.repo-sandbox.kind=registry-preflight io.repo-sandbox.task-id={task_id}\n"
        ),
    )
    .map_err(environment("write registry preflight Dockerfile"))?;
    let cleanup_deadline = DeadlineCancellation::new(std::time::Duration::from_secs(30));
    let mut primary = None;
    let mut push_attempted = buildx_boundary;
    let push = if buildx_boundary {
        let metadata = temporary.path().join("metadata.json");
        quota_command(
            executor,
            vec![
                "buildx".into(),
                "build".into(),
                "--progress=plain".into(),
                "--metadata-file".into(),
                docker_host_path(&metadata),
                "--output".into(),
                format!("type=image,name={probe},push=true"),
                docker_host_path(temporary.path()),
            ],
            cancellation,
        )
    } else {
        let build = quota_command(
            executor,
            vec![
                "build".into(),
                "--file".into(),
                docker_host_path(&temporary.path().join("Dockerfile")),
                "--tag".into(),
                probe.to_string(),
                docker_host_path(temporary.path()),
            ],
            cancellation,
        );
        match build {
            Ok(output) => {
                match quota_command_result("build registry preflight image", &output, cancellation)
                {
                    Ok(()) => {
                        push_attempted = true;
                        quota_command(
                            executor,
                            vec!["push".into(), probe.to_string()],
                            cancellation,
                        )
                    }
                    Err(error) => {
                        primary = Some(error);
                        Err(AppError::Environment(
                            "registry preflight push was not attempted".into(),
                        ))
                    }
                }
            }
            Err(error) => {
                primary = Some(error);
                Err(AppError::Environment(
                    "registry preflight push was not attempted".into(),
                ))
            }
        }
    };
    let reported = match push {
        Ok(output) => {
            let observed = if let Some(digest) = if buildx_boundary {
                registry_metadata_digest(&temporary.path().join("metadata.json"))
                    .or_else(|| registry_push_digest(&output))
            } else {
                registry_push_digest(&output)
            } {
                on_publication(registry_preflight_fact(&probe, digest.clone(), false));
                Some(digest)
            } else {
                if output.exit_code == Some(0) {
                    add_primary_error(
                        &mut primary,
                        AppError::Environment(
                            "registry preflight push succeeded without reporting an immutable digest"
                                .into(),
                        ),
                    );
                }
                None
            };
            if let Err(error) =
                quota_command_result("push registry preflight image", &output, cancellation)
            {
                add_primary_error(&mut primary, error);
            }
            observed
        }
        Err(error) => {
            if primary.is_none() {
                add_primary_error(&mut primary, error);
            }
            None
        }
    };
    if push_attempted {
        match reconcile_registry_manifest(executor, &probe, &cleanup_deadline) {
            Ok(Some(observed)) => {
                if reported.as_ref().is_some_and(|digest| digest != &observed) {
                    add_primary_error(
                        &mut primary,
                        AppError::Environment(format!(
                            "registry preflight digest mismatch: reported {}, observed {observed}",
                            reported.as_ref().expect("checked")
                        )),
                    );
                }
                on_publication(registry_preflight_fact(&probe, observed, true));
            }
            Ok(None) => {
                if primary.is_none() {
                    add_primary_error(
                        &mut primary,
                        AppError::Environment(
                            "registry preflight push reported success but the manifest is absent"
                                .into(),
                        ),
                    );
                }
            }
            Err(error) => add_primary_error(&mut primary, error),
        }
    }
    if !buildx_boundary {
        let local_id = reconcile_quota_resource(
            executor,
            "image",
            probe.as_str(),
            "registry-preflight",
            "io.repo-sandbox.kind",
            "io.repo-sandbox.task-id",
            task_id,
            &cleanup_deadline,
        );
        match local_id {
            Ok(Some(id)) => match quota_command(
                executor,
                vec!["image".into(), "rm".into(), "--force".into(), id],
                &cleanup_deadline,
            ) {
                Ok(output) => {
                    if let Err(error) = quota_command_result(
                        "remove local registry preflight image",
                        &output,
                        &cleanup_deadline,
                    ) {
                        add_primary_error(&mut primary, error);
                    }
                }
                Err(error) => add_primary_error(&mut primary, error),
            },
            Ok(None) => {}
            Err(error) => add_primary_error(&mut primary, error),
        }
    }
    primary.map_or(Ok(()), Err)
}

fn reconcile_registry_manifest(
    executor: &impl ProcessExecutor,
    probe: &ImageRef,
    cancellation: &dyn Cancellation,
) -> Result<Option<repo_sandbox_core::build::ImageDigest>, AppError> {
    let mut consecutive_absent = 0;
    let mut last_error = String::new();
    for _ in 0..20 {
        if cancellation.is_cancelled() {
            break;
        }
        match quota_command(
            executor,
            vec![
                "buildx".into(),
                "imagetools".into(),
                "inspect".into(),
                probe.to_string(),
            ],
            cancellation,
        ) {
            Ok(output) if output.exit_code == Some(0) => {
                return registry_push_digest(&output).map(Some).ok_or_else(|| {
                    AppError::Environment(
                        "registry preflight reconciliation omitted the immutable digest".into(),
                    )
                });
            }
            Ok(output) if registry_manifest_absent(&output.stderr) => {
                consecutive_absent += 1;
                if consecutive_absent >= 3 {
                    return Ok(None);
                }
            }
            Ok(output) => {
                consecutive_absent = 0;
                last_error = output.stderr.trim().to_owned();
            }
            Err(error) => {
                consecutive_absent = 0;
                last_error = error.to_string();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    Err(AppError::Environment(format!(
        "registry preflight reconciliation did not stabilize: {last_error}"
    )))
}

fn registry_preflight_fact(
    reference: &ImageRef,
    digest: repo_sandbox_core::build::ImageDigest,
    verified: bool,
) -> RemotePublicationFact {
    RemotePublicationFact {
        kind: PublicationFactKind::RegistryPreflightStaging,
        reference: reference.clone(),
        digest,
        verified,
        finality: PublicationFinality::Staging,
    }
}

fn registry_manifest_absent(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("manifest unknown") || stderr.contains("no such manifest")
}

fn add_primary_error(primary: &mut Option<AppError>, error: AppError) {
    *primary = Some(match primary.take() {
        Some(existing) => AppError::Environment(format!("{existing}; {error}")),
        None => error,
    });
}

fn registry_metadata_digest(path: &Path) -> Option<repo_sandbox_core::build::ImageDigest> {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    repo_sandbox_core::build::ImageDigest::new(
        value.get("containerimage.digest")?.as_str()?.to_owned(),
    )
    .ok()
}

fn registry_push_digest(
    output: &crate::buildkit::ProcessOutput,
) -> Option<repo_sandbox_core::build::ImageDigest> {
    for text in [&output.stdout, &output.stderr] {
        for (offset, _) in text.match_indices("sha256:") {
            let end = offset + 7 + 64;
            if end <= text.len()
                && let Ok(digest) =
                    repo_sandbox_core::build::ImageDigest::new(text[offset..end].to_owned())
            {
                return Some(digest);
            }
        }
    }
    None
}

fn preflight_writable_layer_quota(
    executor: &impl ProcessExecutor,
    storage_mb: u32,
    cancellation: &dyn Cancellation,
) -> Result<(), AppError> {
    preflight_writable_layer_quota_with_identity(executor, storage_mb, cancellation, &task_id())
}

fn preflight_writable_layer_quota_with_identity(
    executor: &impl ProcessExecutor,
    storage_mb: u32,
    cancellation: &dyn Cancellation,
    identity: &str,
) -> Result<(), AppError> {
    let temporary = tempfile::Builder::new()
        .prefix("repo-sandbox-quota-preflight-")
        .tempdir()
        .map_err(environment("create quota preflight directory"))?;
    let image = format!("repo-sandbox-quota-probe:{identity}");
    let container = format!("repo-sandbox-quota-probe-{identity}");
    let kind_label_key = "io.repo-sandbox.kind";
    let task_label_key = "io.repo-sandbox.task-id";
    let kind_label = format!("{kind_label_key}=quota-probe");
    let task_label = format!("{task_label_key}={identity}");
    let cleanup_deadline = DeadlineCancellation::new(std::time::Duration::from_secs(30));
    let dockerfile = temporary.path().join("Dockerfile");
    fs::write(
        &dockerfile,
        format!("FROM scratch\nLABEL {kind_label} {task_label}\n"),
    )
    .map_err(environment("write quota probe Dockerfile"))?;
    let build = quota_command(
        executor,
        vec![
            "build".into(),
            "--file".into(),
            docker_host_path(&dockerfile),
            "--tag".into(),
            image.clone(),
            docker_host_path(temporary.path()),
        ],
        cancellation,
    );
    let (mut primary, build_succeeded) = match build {
        Ok(output) => (
            quota_command_result("build quota probe image", &output, cancellation),
            output.exit_code == Some(0),
        ),
        Err(error) => (Err(error), false),
    };
    let image_id = reconcile_quota_resource(
        executor,
        "image",
        &image,
        "quota-probe",
        kind_label_key,
        task_label_key,
        identity,
        &cleanup_deadline,
    )?;
    if build_succeeded && image_id.is_none() {
        primary = Err(AppError::Environment(
            "quota probe image disappeared after successful build".into(),
        ));
    }
    if primary.is_ok() {
        let create = quota_command(
            executor,
            vec![
                "container".into(),
                "create".into(),
                "--name".into(),
                container.clone(),
                "--label".into(),
                kind_label.clone(),
                "--label".into(),
                task_label.clone(),
                "--storage-opt".into(),
                format!("size={storage_mb}m"),
                image.clone(),
                "/bin/true".into(),
            ],
            cancellation,
        );
        primary = match create {
            Ok(output) => {
                quota_command_result("create quota probe container", &output, cancellation)
            }
            Err(error) => Err(error),
        };
    }
    // Container creation can return interruption/error before the daemon's
    // eventual object becomes visible. Reconcile to a stable absence or an
    // exact doubly-labelled owned ID before deciding whether removal is safe.
    let container_id = match reconcile_quota_resource(
        executor,
        "container",
        &container,
        "quota-probe",
        kind_label_key,
        task_label_key,
        identity,
        &cleanup_deadline,
    ) {
        Ok(id) => id,
        Err(error) => {
            primary = Err(match primary {
                Ok(()) => error,
                Err(primary) => AppError::Environment(format!("{primary}; {error}")),
            });
            None
        }
    };
    let mut cleanup_errors = Vec::new();
    for (kind, id) in [("container", container_id), ("image", image_id)] {
        if let Some(id) = id {
            match quota_command(
                executor,
                vec![kind.into(), "rm".into(), "--force".into(), id],
                &cleanup_deadline,
            ) {
                Ok(output) if output.exit_code == Some(0) => {}
                Ok(output) => cleanup_errors.push(format!(
                    "Docker quota probe cleanup failed: {}",
                    output.stderr.trim()
                )),
                Err(error) => cleanup_errors.push(error.to_string()),
            }
        }
    }
    match (primary, cleanup_errors.is_empty()) {
        (Ok(()), true) => Ok(()),
        (Err(error), true) => Err(error),
        (Ok(()), false) => Err(AppError::Environment(format!(
            "quota preflight cleanup failed: {}",
            cleanup_errors.join("; ")
        ))),
        (Err(error), false) => Err(AppError::Environment(format!(
            "{error}; quota preflight cleanup failed: {}",
            cleanup_errors.join("; ")
        ))),
    }
}

fn quota_command(
    executor: &impl ProcessExecutor,
    args: Vec<String>,
    cancellation: &dyn Cancellation,
) -> Result<crate::buildkit::ProcessOutput, AppError> {
    executor
        .execute(
            &ProcessInvocation {
                program: "docker".into(),
                args,
                current_dir: None,
            },
            cancellation,
        )
        .map_err(environment("execute writable-layer quota preflight"))
}

fn quota_command_result(
    operation: &str,
    output: &crate::buildkit::ProcessOutput,
    cancellation: &dyn Cancellation,
) -> Result<(), AppError> {
    if output.interrupted || cancellation.is_cancelled() {
        Err(AppError::Environment(
            "writable-layer quota preflight was cancelled or timed out".into(),
        ))
    } else if output.exit_code == Some(0) {
        Ok(())
    } else {
        Err(AppError::Environment(format!(
            "Docker daemon does not support the required writable-layer quota during {operation}: {}",
            output.stderr.trim()
        )))
    }
}

#[allow(clippy::too_many_arguments)] // Ownership requires both labels, exact name, kind, and deadline.
fn reconcile_quota_resource(
    executor: &impl ProcessExecutor,
    kind: &str,
    name: &str,
    expected_kind: &str,
    kind_label_key: &str,
    task_label_key: &str,
    owner: &str,
    cancellation: &dyn Cancellation,
) -> Result<Option<String>, AppError> {
    let format = format!(
        "{{{{.Id}}}}|{{{{index .Config.Labels \"{kind_label_key}\"}}}}|{{{{index .Config.Labels \"{task_label_key}\"}}}}"
    );
    let mut consecutive_absent = 0;
    for _ in 0..20 {
        if cancellation.is_cancelled() {
            return Err(AppError::Environment(format!(
                "quota probe {kind} reconciliation was cancelled or timed out"
            )));
        }
        let output = match quota_command(
            executor,
            vec![
                kind.into(),
                "inspect".into(),
                "--format".into(),
                format.clone(),
                name.into(),
            ],
            cancellation,
        ) {
            Ok(output) => output,
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(25));
                continue;
            }
        };
        if output.exit_code == Some(0) {
            let mut identity = output.stdout.trim().split('|');
            let (id, actual_kind, actual_owner) = (
                identity.next().unwrap_or_default(),
                identity.next().unwrap_or_default(),
                identity.next().unwrap_or_default(),
            );
            if identity.next().is_some()
                || actual_kind != expected_kind
                || actual_owner != owner
                || !id.starts_with("sha256:")
            {
                return Err(AppError::Environment(format!(
                    "refuse to remove foreign quota probe {kind} named {name}"
                )));
            }
            return Ok(Some(id.to_owned()));
        }
        let stderr = output.stderr.to_ascii_lowercase();
        let absent = match kind {
            "container" => stderr.contains("no such container"),
            "image" => stderr.contains("no such image") || stderr.contains("no such object"),
            _ => false,
        };
        if !absent {
            return Err(AppError::Environment(format!(
                "inspect quota probe {kind} failed: {}",
                output.stderr.trim()
            )));
        }
        consecutive_absent += 1;
        if consecutive_absent >= 3 {
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    Err(AppError::Environment(format!(
        "quota probe {kind} reconciliation did not stabilize"
    )))
}

#[cfg(not(windows))]
fn workflow_available_space(
    repository: &Path,
    cancellation: &dyn Cancellation,
) -> Result<u64, AppError> {
    workflow_available_space_with(&SystemProcessExecutor, repository, cancellation)
}

#[cfg(not(windows))]
fn workflow_available_space_with(
    executor: &impl ProcessExecutor,
    repository: &Path,
    cancellation: &dyn Cancellation,
) -> Result<u64, AppError> {
    let invocation = ProcessInvocation {
        program: "df".into(),
        args: vec!["-Pk".into(), repository.to_string_lossy().into_owned()],
        current_dir: None,
    };
    let output = executor
        .execute(&invocation, cancellation)
        .map_err(environment("execute disk preflight"))?;
    if output.interrupted {
        return Err(AppError::Environment("disk preflight was cancelled".into()));
    }
    if output.exit_code != Some(0) {
        return Err(AppError::Environment(format!(
            "disk preflight failed: {}",
            output.stderr.trim()
        )));
    }
    parse_df_available_space(&output.stdout)
}

#[cfg(not(windows))]
fn parse_df_available_space(stdout: &str) -> Result<u64, AppError> {
    let line = stdout
        .lines()
        .rfind(|line| !line.trim().is_empty())
        .ok_or_else(|| AppError::Environment("disk preflight returned no data".into()))?;
    let kib = line
        .split_whitespace()
        .nth(3)
        .ok_or_else(|| AppError::Environment("disk preflight returned invalid data".into()))?
        .parse::<u64>()
        .map_err(|error| AppError::Environment(format!("disk preflight invalid bytes: {error}")))?;
    kib.checked_mul(1024)
        .ok_or_else(|| AppError::Environment("disk preflight byte count overflowed".into()))
}

#[cfg(windows)]
fn workflow_available_space(
    repository: &Path,
    cancellation: &dyn Cancellation,
) -> Result<u64, AppError> {
    if cancellation.is_cancelled() {
        return Err(AppError::Environment("disk preflight was cancelled".into()));
    }
    SystemDoctorProbe
        .available_space(repository)
        .map_err(environment("disk preflight"))
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

fn clean_failed_cache_export(path: &Path, primary: AppError) -> AppError {
    match fs::remove_dir_all(path) {
        Ok(()) => primary,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => primary,
        Err(error) => AppError::Environment(format!(
            "{primary}; remove failed task cache export {}: {error}",
            path.display()
        )),
    }
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

fn registration_failure_with_safe_retention(primary: AppError, image_id: &ImageRef) -> AppError {
    AppError::Environment(format!(
        "{primary}; local task image {image_id} was safely retained because concurrent identical builds may share that immutable image ID"
    ))
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
    let _lease = CacheLease::exclusive(cache, cancellation)?;
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

struct CacheLease {
    _file: fs::File,
}

impl CacheLease {
    fn shared(cache: &Path, cancellation: &dyn Cancellation) -> Result<Self, AppError> {
        Self::acquire(cache, cancellation, true)
    }

    fn exclusive(cache: &Path, cancellation: &dyn Cancellation) -> Result<Self, AppError> {
        Self::acquire(cache, cancellation, false)
    }

    fn acquire(
        cache: &Path,
        cancellation: &dyn Cancellation,
        shared: bool,
    ) -> Result<Self, AppError> {
        let path = cache.join(".rotation.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            #[cfg(any(target_os = "linux", target_os = "android"))]
            const O_NOFOLLOW: i32 = 0x0002_0000;
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            const O_NOFOLLOW: i32 = 0x0000_0100;
            options.custom_flags(O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        let file = options
            .open(path)
            .map_err(environment("open cache lease"))?;
        let metadata = file
            .metadata()
            .map_err(environment("inspect cache lease"))?;
        if !metadata.is_file() || !state_file_has_single_link(&file, &metadata) {
            return Err(AppError::Environment(
                "cache lease control path must be a single-link regular file".into(),
            ));
        }
        loop {
            let locked = if shared {
                file.try_lock_shared()
            } else {
                file.try_lock()
            };
            match locked {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) if !cancellation.is_cancelled() => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(AppError::Environment(
                        "workflow cancelled while waiting for cache lease".into(),
                    ));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(AppError::Environment(format!("lock cache lease: {error}")));
                }
            }
        }
    }
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
    use std::hash::{BuildHasher, RandomState};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_TICK: AtomicU64 = AtomicU64::new(0);
    static PROCESS_NONCE: OnceLock<u64> = OnceLock::new();
    let process_nonce = *PROCESS_NONCE.get_or_init(|| {
        RandomState::new().hash_one((
            std::process::id(),
            SystemTime::now(),
            &PROCESS_NONCE as *const _ as usize,
        ))
    });
    let wall_clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let mut previous = LAST_TICK.load(Ordering::Relaxed);
    let unique = loop {
        let candidate = wall_clock.max(previous.saturating_add(1));
        match LAST_TICK.compare_exchange_weak(
            previous,
            candidate,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break candidate,
            Err(actual) => previous = actual,
        }
    };
    format!("{}-{process_nonce:016x}-{unique}", std::process::id())
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

#[derive(Clone)]
struct StateLayoutGuard {
    repository: PathBuf,
    components: std::sync::Arc<Vec<BoundStateComponent>>,
}

struct BoundStateComponent {
    path: PathBuf,
    identity: StateIdentity,
    _handle: fs::File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StateIdentity {
    first: u64,
    second: u64,
}

impl StateLayoutGuard {
    fn capture(repository: &Path, components: &[PathBuf]) -> Result<Self, AppError> {
        Self::capture_with_hook(repository, components, |_| {})
    }

    fn capture_with_hook(
        repository: &Path,
        components: &[PathBuf],
        mut after_open: impl FnMut(&Path),
    ) -> Result<Self, AppError> {
        Ok(Self {
            repository: repository.to_path_buf(),
            components: std::sync::Arc::new(
                components
                    .iter()
                    .map(|path| {
                        // Open the directory itself with no-follow semantics first.
                        // Identity and boundary checks are derived from that fixed
                        // handle, never from a pathname observed before opening it.
                        let handle = bind_state_directory(path)?;
                        let identity = state_identity_from_handle(&handle)?;
                        after_open(path);
                        validate_bound_state_component(repository, path, identity)?;
                        Ok(BoundStateComponent {
                            path: path.clone(),
                            identity,
                            _handle: handle,
                        })
                    })
                    .collect::<Result<_, AppError>>()?,
            ),
        })
    }

    fn ensure(&self) -> Result<(), AppError> {
        for component in self.components.iter() {
            validate_state_component(&self.repository, &component.path)?;
            if state_identity(&component.path)? != component.identity {
                return Err(AppError::Environment(format!(
                    "workflow state component changed during execution: {}",
                    component.path.display()
                )));
            }
        }
        Ok(())
    }

    fn bound_path(&self, path: &Path) -> Result<PathBuf, AppError> {
        #[cfg(windows)]
        {
            self.ensure()?;
            Ok(path.to_path_buf())
        }
        #[cfg(unix)]
        {
            let component = self
                .components
                .iter()
                .filter_map(|component| {
                    path.strip_prefix(&component.path)
                        .ok()
                        .map(|relative| (component, relative))
                })
                .max_by_key(|(component, _)| component.path.components().count())
                .ok_or_else(|| {
                    AppError::Environment(format!(
                        "path is outside the bound workflow state: {}",
                        path.display()
                    ))
                })?;
            bound_directory_path(&component.0._handle).map(|root| root.join(component.1))
        }
    }
}

#[cfg(target_os = "linux")]
fn bound_directory_path(handle: &fs::File) -> Result<PathBuf, AppError> {
    use std::os::fd::AsRawFd;
    Ok(PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        handle.as_raw_fd()
    )))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn bound_directory_path(handle: &fs::File) -> Result<PathBuf, AppError> {
    use std::os::fd::AsRawFd;
    Ok(PathBuf::from(format!("/dev/fd/{}", handle.as_raw_fd())))
}

#[cfg(unix)]
fn bind_state_directory(path: &Path) -> Result<fs::File, AppError> {
    use std::os::unix::fs::OpenOptionsExt;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const O_DIRECTORY: i32 = 0x0001_0000;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const O_NOFOLLOW: i32 = 0x0002_0000;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const O_DIRECTORY: i32 = 0x0010_0000;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const O_NOFOLLOW: i32 = 0x0000_0100;
    OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW)
        .open(path)
        .map_err(environment("open bound workflow state directory"))
}

#[cfg(windows)]
fn bind_state_directory(path: &Path) -> Result<fs::File, AppError> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .read(true)
        // Excluding FILE_SHARE_DELETE binds the pathname for the guard lifetime:
        // Windows cannot rename or replace a component behind an open handle.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .map_err(environment("open bound workflow state directory"))
}

#[cfg(unix)]
fn state_identity(path: &Path) -> Result<StateIdentity, AppError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path).map_err(environment("bind workflow state"))?;
    Ok(StateIdentity {
        first: metadata.dev(),
        second: metadata.ino(),
    })
}

#[cfg(unix)]
fn state_identity_from_handle(handle: &fs::File) -> Result<StateIdentity, AppError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = handle
        .metadata()
        .map_err(environment("inspect bound workflow state directory"))?;
    if !metadata.is_dir() {
        return Err(AppError::Environment(
            "bound workflow state component is not a directory".into(),
        ));
    }
    Ok(StateIdentity {
        first: metadata.dev(),
        second: metadata.ino(),
    })
}

#[cfg(windows)]
fn state_identity(path: &Path) -> Result<StateIdentity, AppError> {
    use std::os::windows::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path).map_err(environment("bind workflow state"))?;
    Ok(StateIdentity {
        // Creation time is stable across writes and changes when a path is replaced;
        // reparse points are independently rejected above.
        first: metadata.creation_time(),
        second: u64::from(metadata.file_attributes()),
    })
}

#[cfg(windows)]
fn state_identity_from_handle(handle: &fs::File) -> Result<StateIdentity, AppError> {
    use std::os::windows::fs::MetadataExt;
    let metadata = handle
        .metadata()
        .map_err(environment("inspect bound workflow state directory"))?;
    if is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(AppError::Environment(
            "bound workflow state component must be a real directory".into(),
        ));
    }
    Ok(StateIdentity {
        first: metadata.creation_time(),
        second: u64::from(metadata.file_attributes()),
    })
}

fn validate_bound_state_component(
    repository: &Path,
    path: &Path,
    handle_identity: StateIdentity,
) -> Result<(), AppError> {
    validate_state_component(repository, path)?;
    if state_identity(path)? != handle_identity {
        return Err(AppError::Environment(format!(
            "workflow state component changed while it was being bound: {}",
            path.display()
        )));
    }
    Ok(())
}

fn state_component_paths(state: &Path, base: &Path) -> Vec<PathBuf> {
    let mut paths = vec![base.to_path_buf(), base.join("remotes")];
    if state != base {
        paths.push(state.to_path_buf());
    }
    for leaf in ["tasks", "cache", "reports", "artifacts"] {
        paths.push(state.join(leaf));
    }
    paths
}

fn prepare_state_layout(repository: &Path, state: &Path) -> Result<StateLayoutGuard, AppError> {
    prepare_state_layout_with_hook(repository, state, || {})
}

fn prepare_leased_workflow_state(
    repository: &Path,
    state: &Path,
    task_id: &str,
) -> Result<(StateLayoutGuard, WorkflowLease, ManifestJournal), AppError> {
    let guard = prepare_state_layout(repository, state)?;
    // Acquire the shared lease before the first journal write or any potentially
    // long preflight probe, closing cleanup's state-initialization window.
    let lease = WorkflowLease::shared(&guard.bound_path(&repository.join(".repo-sandbox"))?)?;
    let journal = ManifestJournal::create(state, task_id, guard.clone())?;
    Ok((guard, lease, journal))
}

fn prepare_state_layout_with_hook(
    repository: &Path,
    state: &Path,
    after_validation: impl FnOnce(),
) -> Result<StateLayoutGuard, AppError> {
    // Complete the read-only validation pass before creating any component.
    validate_state_root(repository, state)?;
    after_validation();
    // Bind the exact components again at the write boundary. The retained
    // identities are checked before every later cache, journal, and report I/O.
    validate_state_root(repository, state)?;
    let base = repository.join(".repo-sandbox");
    let paths = state_component_paths(state, &base);
    for path in &paths {
        if !path.exists() {
            match fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(environment("create workflow state component")(error)),
            }
        }
        validate_state_component(repository, path)?;
    }
    StateLayoutGuard::capture(repository, &paths)
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

enum ReportDestinationPlan {
    State { path: PathBuf },
    External { path: PathBuf },
}

enum ReportDestination {
    State { path: PathBuf },
    External(ExternalReportGuard),
}

impl ReportDestination {
    fn prepare(plan: ReportDestinationPlan) -> Result<Self, AppError> {
        match plan {
            ReportDestinationPlan::State { path } => Ok(Self::State { path }),
            ReportDestinationPlan::External { path } => {
                ExternalReportGuard::prepare(&path).map(Self::External)
            }
        }
    }

    fn bound_path(&self, state: Option<&StateLayoutGuard>) -> Result<PathBuf, AppError> {
        match self {
            Self::State { path } => state
                .ok_or_else(|| AppError::Environment("workflow state is not bound".into()))?
                .bound_path(path),
            Self::External(guard) => guard.bound_path(),
        }
    }

    fn exists(&self, state: Option<&StateLayoutGuard>) -> Result<bool, AppError> {
        match self.bound_path(state) {
            Ok(path) => Ok(path.exists()),
            Err(_) if matches!(self, Self::State { .. }) && state.is_none() => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn classify_report_destination(
    state: &Path,
    path: &Path,
) -> Result<ReportDestinationPlan, AppError> {
    let resolved_state = resolved_future_path(state)?;
    let resolved_path = resolved_future_path(path)?;
    if let Ok(relative) = resolved_path.strip_prefix(&resolved_state) {
        Ok(ReportDestinationPlan::State {
            path: state.join(relative),
        })
    } else {
        Ok(ReportDestinationPlan::External {
            path: resolved_path,
        })
    }
}

#[cfg(test)]
fn path_is_within_state(state: &Path, path: &Path) -> Result<bool, AppError> {
    Ok(resolved_future_path(path)?.starts_with(resolved_future_path(state)?))
}

#[derive(Debug)]
struct ExternalReportGuard {
    path: PathBuf,
    _parent: PathBuf,
    _handle: Option<fs::File>,
    created: Vec<PathBuf>,
}

/// Pins an OCI destination's parent for the complete workflow. On Unix every
/// staging/final operation is addressed through the retained directory fd; on
/// Windows the retained handle excludes delete sharing, so the parent cannot
/// be renamed or replaced underneath the workflow.
#[derive(Debug)]
struct ExternalOciGuard {
    path: PathBuf,
    _parent: PathBuf,
    handle: Option<fs::File>,
    created: Vec<PathBuf>,
    _output_reservation: OutputReservation,
}

impl ExternalOciGuard {
    fn prepare(output: &Path) -> Result<Self, AppError> {
        Self::prepare_inner(output).map_err(|error| {
            AppError::Configuration(format!(
                "invalid OCI layout destination parent for {}: {error}",
                output.display()
            ))
        })
    }

    fn prepare_inner(output: &Path) -> Result<Self, AppError> {
        let output = resolved_future_path(output)?;
        if output.exists() {
            return Err(AppError::Configuration(format!(
                "OCI layout already exists: {}",
                output.display()
            )));
        }
        let parent = output
            .parent()
            .ok_or_else(|| {
                AppError::Configuration(format!(
                    "OCI layout path has no parent: {}",
                    output.display()
                ))
            })?
            .to_path_buf();
        let output_reservation = OutputReservation::oci(&output)?;
        let parent_reservation = OutputReservation::create(&parent, "OCI layout parent")?;
        let created = create_report_parent(&parent)?;
        let handle = match bind_report_parent(&parent).map_err(|error| {
            AppError::Configuration(format!(
                "cannot bind OCI layout destination parent {}: {error}",
                parent.display()
            ))
        }) {
            Ok(handle) => handle,
            Err(error) => {
                rollback_created_directories(&created);
                return Err(error);
            }
        };
        let validation = (|| {
            let current_parent = resolved_future_path(&parent)?;
            let bound_identity = state_identity_from_handle(&handle)?;
            let current_identity = state_identity(&parent)?;
            if current_parent != parent || bound_identity != current_identity {
                return Err(AppError::Configuration(format!(
                    "OCI layout destination changed while it was being bound: {}",
                    parent.display()
                )));
            }
            Ok(())
        })();
        if let Err(error) = validation {
            drop(handle);
            rollback_created_directories(&created);
            return Err(error);
        }
        drop(parent_reservation);
        Ok(Self {
            path: output,
            _parent: parent,
            handle: Some(handle),
            created,
            _output_reservation: output_reservation,
        })
    }

    fn bound_path(&self) -> Result<PathBuf, AppError> {
        let name = self.path.file_name().ok_or_else(|| {
            AppError::Configuration(format!(
                "OCI layout path has no file name: {}",
                self.path.display()
            ))
        })?;
        #[cfg(unix)]
        {
            bound_directory_path(self.handle.as_ref().expect("OCI guard handle is live"))
                .map(|parent| parent.join(name))
        }
        #[cfg(windows)]
        {
            Ok(self._parent.join(name))
        }
    }
}

impl Drop for ExternalOciGuard {
    fn drop(&mut self) {
        drop(self.handle.take());
        rollback_created_directories(&self.created);
    }
}

impl ExternalReportGuard {
    fn prepare(report: &Path) -> Result<Self, AppError> {
        let report = resolved_future_path(report).map_err(|error| {
            AppError::Configuration(format!(
                "invalid report destination parent for {}: {error}",
                report.display()
            ))
        })?;
        let report = report.as_path();
        let parent = report.parent().ok_or_else(|| {
            AppError::Configuration(format!("report path has no parent: {}", report.display()))
        })?;
        // Keep parent creation/rollback exclusive across processes, including
        // workflows targeting different filenames in the same new directory.
        let parent_reservation = OutputReservation::create(parent, "report parent")?;
        let created = create_report_parent(parent)?;
        let handle = match bind_report_parent(parent).map_err(|error| {
            AppError::Configuration(format!(
                "cannot bind report destination parent {}: {error}",
                parent.display()
            ))
        }) {
            Ok(handle) => handle,
            Err(error) => {
                rollback_created_directories(&created);
                return Err(error);
            }
        };
        let validation = (|| {
            let current_parent = resolved_future_path(parent)?;
            let bound_identity = state_identity_from_handle(&handle)?;
            let current_identity = state_identity(parent)?;
            if current_parent != parent || bound_identity != current_identity {
                return Err(AppError::Configuration(format!(
                    "report destination changed while it was being bound: {}",
                    parent.display()
                )));
            }
            Ok(())
        })();
        if let Err(error) = validation {
            drop(handle);
            rollback_created_directories(&created);
            return Err(error);
        }
        drop(parent_reservation);
        Ok(Self {
            path: report.to_path_buf(),
            _parent: parent.to_path_buf(),
            _handle: Some(handle),
            created,
        })
    }

    fn bound_path(&self) -> Result<PathBuf, AppError> {
        let name = self.path.file_name().ok_or_else(|| {
            AppError::Configuration(format!(
                "report path has no file name: {}",
                self.path.display()
            ))
        })?;
        #[cfg(unix)]
        {
            bound_directory_path(self._handle.as_ref().expect("report guard handle is live"))
                .map(|parent| parent.join(name))
        }
        #[cfg(windows)]
        {
            // The retained parent handle excludes FILE_SHARE_DELETE, so the
            // pathname cannot be renamed or replaced until this guard drops.
            Ok(self._parent.join(name))
        }
    }
}

impl Drop for ExternalReportGuard {
    fn drop(&mut self) {
        drop(self._handle.take());
        rollback_created_directories(&self.created);
    }
}

fn create_report_parent(parent: &Path) -> Result<Vec<PathBuf>, AppError> {
    let mut missing = Vec::new();
    let mut cursor = parent;
    while !cursor.exists() {
        missing.push(cursor.to_path_buf());
        cursor = cursor.parent().ok_or_else(|| {
            AppError::Configuration(format!(
                "report path has no existing ancestor: {}",
                parent.display()
            ))
        })?;
    }
    let mut created = Vec::new();
    for path in missing.iter().rev() {
        match fs::create_dir(path) {
            Ok(()) => created.push(path.clone()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                rollback_created_directories(&created);
                return Err(AppError::Configuration(format!(
                    "cannot prepare report destination {}: {error}",
                    parent.display()
                )));
            }
        }
    }
    Ok(created)
}

fn rollback_created_directories(created: &[PathBuf]) {
    for path in created.iter().rev() {
        // remove_dir is deliberately non-recursive: concurrent/user content
        // makes rollback retain the directory instead of deleting that data.
        let _ = fs::remove_dir(path);
    }
}

#[cfg(unix)]
fn bind_report_parent(parent: &Path) -> Result<fs::File, AppError> {
    use std::os::fd::AsRawFd;
    let handle = bind_state_directory(parent).map_err(|error| {
        AppError::Configuration(format!(
            "cannot bind report destination {}: {error}",
            parent.display()
        ))
    })?;
    unsafe extern "C" {
        fn faccessat(dirfd: i32, path: *const std::ffi::c_char, mode: i32, flags: i32) -> i32;
    }
    const W_OK: i32 = 2;
    let current = c".";
    // SAFETY: current is a static NUL-terminated string and the directory file
    // descriptor remains open for this call and the guard lifetime.
    if unsafe { faccessat(handle.as_raw_fd(), current.as_ptr(), W_OK, 0) } != 0 {
        return Err(AppError::Configuration(format!(
            "report destination is not writable {}: {}",
            parent.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(handle)
}

#[cfg(windows)]
fn bind_report_parent(parent: &Path) -> Result<fs::File, AppError> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_LIST_DIRECTORY: u32 = 0x0001;
    const FILE_ADD_FILE: u32 = 0x0002;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let handle = OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_ADD_FILE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(parent)
        .map_err(|error| {
            AppError::Configuration(format!(
                "cannot bind writable report destination {}: {error}",
                parent.display()
            ))
        })?;
    let metadata = handle.metadata().map_err(|error| {
        AppError::Configuration(format!(
            "cannot inspect report destination {}: {error}",
            parent.display()
        ))
    })?;
    if is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(AppError::Configuration(format!(
            "report destination parent must be a real directory: {}",
            parent.display()
        )));
    }
    Ok(handle)
}

fn write_state_file(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        const O_NOFOLLOW: i32 = 0x0002_0000;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        const O_NOFOLLOW: i32 = 0x0000_0100;
        options.custom_flags(O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options
        .open(path)
        .map_err(environment("open bound workflow state file"))?;
    let metadata = file
        .metadata()
        .map_err(environment("inspect bound workflow state file"))?;
    if is_link_or_reparse(&metadata)
        || !metadata.is_file()
        || !state_file_has_single_link(&file, &metadata)
    {
        return Err(AppError::Environment(
            "workflow state file must be a single-link regular file".into(),
        ));
    }
    file.set_len(0)
        .map_err(environment("truncate bound workflow state file"))?;
    file.seek(SeekFrom::Start(0))
        .map_err(environment("seek bound workflow state file"))?;
    file.write_all(bytes)
        .map_err(environment("write bound workflow state file"))?;
    file.sync_all()
        .map_err(environment("sync bound workflow state file"))
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
        let absolute = resolved_future_path(output)?;
        Self::create_identity(&absolute.to_string_lossy(), output, description)
    }

    fn create_identity(
        identity: &str,
        display: &Path,
        description: &str,
    ) -> Result<Self, AppError> {
        let file = Self::open_identity(identity, display, description)?;
        file.try_lock().map_err(|error| {
            AppError::Configuration(format!(
                "cannot reserve {description} {}: {error}",
                display.display()
            ))
        })?;
        Ok(Self { _file: file })
    }

    fn wait_identity(
        identity: &str,
        display: &Path,
        description: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<Self, AppError> {
        let file = Self::open_identity(identity, display, description)?;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) if !cancellation.is_cancelled() => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(AppError::Environment(format!(
                        "workflow cancelled while waiting to reserve {description} {}",
                        display.display()
                    )));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(AppError::Environment(format!(
                        "lock {description} {}: {error}",
                        display.display()
                    )));
                }
            }
        }
    }

    fn open_identity(
        identity: &str,
        display: &Path,
        description: &str,
    ) -> Result<fs::File, AppError> {
        let mut digest = Sha256::new();
        digest.update(identity.as_bytes());
        let root = output_reservation_root()?;
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
                    display.display()
                ))
            })?;
        Ok(file)
    }
}

fn output_reservation_root() -> Result<PathBuf, AppError> {
    #[cfg(unix)]
    let root = {
        let uid = effective_uid();
        std::env::temp_dir().join(format!("repo-sandbox-{uid}-output-reservations-v1"))
    };
    #[cfg(windows)]
    let root = std::env::temp_dir().join("repo-sandbox-output-reservations-v1");

    if !root.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&root)
                .or_else(|error| {
                    (error.kind() == std::io::ErrorKind::AlreadyExists)
                        .then_some(())
                        .ok_or(error)
                })
                .map_err(environment("create private output reservation directory"))?;
        }
        #[cfg(windows)]
        fs::create_dir(&root)
            .or_else(|error| {
                (error.kind() == std::io::ErrorKind::AlreadyExists)
                    .then_some(())
                    .ok_or(error)
            })
            .map_err(environment("create private output reservation directory"))?;
    }
    let metadata = fs::symlink_metadata(&root)
        .map_err(environment("inspect private output reservation directory"))?;
    if is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(AppError::Environment(
            "output reservation root must be a real private directory".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let uid = effective_uid();
        if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(AppError::Environment(
                "output reservation root must be owned by the current user with mode 0700".into(),
            ));
        }
    }
    Ok(root)
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no arguments or memory-safety preconditions.
    unsafe { geteuid() }
}

struct ManifestJournal {
    root: PathBuf,
    task_id: String,
    state: StateLayoutGuard,
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
    fn create(state: &Path, task_id: &str, guard: StateLayoutGuard) -> Result<Self, AppError> {
        guard.ensure()?;
        let root = guard.bound_path(&state.join("tasks"))?;
        let journal = Self {
            root,
            task_id: task_id.into(),
            state: guard,
        };
        journal.append(&[])?;
        Ok(journal)
    }

    fn append(&self, candidates: &[CleanCandidate]) -> Result<(), AppError> {
        self.state.ensure()?;
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
        publish_journal_event(&temporary, &final_path, &self.root)
    }
}

#[cfg(unix)]
fn publish_journal_event(temporary: &Path, final_path: &Path, root: &Path) -> Result<(), AppError> {
    fs::rename(temporary, final_path).map_err(environment("publish task manifest event"))?;
    bind_state_directory(root)?
        .sync_all()
        .map_err(environment("sync task manifest directory"))
}

#[cfg(windows)]
fn publish_journal_event(
    temporary: &Path,
    final_path: &Path,
    _root: &Path,
) -> Result<(), AppError> {
    use std::os::windows::ffi::OsStrExt;
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, target: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;
    let existing = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = final_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both UTF-16 buffers are NUL-terminated and live for the call.
    if unsafe { MoveFileExW(existing.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        Err(environment("publish durable task manifest event")(
            std::io::Error::last_os_error(),
        ))
    } else {
        Ok(())
    }
}

fn next_journal_sequence(root: &Path) -> Result<u64, AppError> {
    fs::create_dir_all(root).map_err(environment("create journal directory"))?;
    let path = root.join(".sequence");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        const O_NOFOLLOW: i32 = 0x0002_0000;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        const O_NOFOLLOW: i32 = 0x0000_0100;
        options.custom_flags(O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options
        .open(&path)
        .map_err(environment("open journal sequence"))?;
    let metadata = file
        .metadata()
        .map_err(environment("inspect journal sequence"))?;
    if is_link_or_reparse(&metadata)
        || !metadata.is_file()
        || !state_file_has_single_link(&file, &metadata)
    {
        return Err(AppError::Environment(
            "journal sequence must be a single-link regular file".into(),
        ));
    }
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

#[cfg(unix)]
fn state_file_has_single_link(_file: &fs::File, metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() == 1
}

#[cfg(windows)]
fn state_file_has_single_link(file: &fs::File, _metadata: &fs::Metadata) -> bool {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    struct FileInformation {
        attributes: u32,
        creation: FileTime,
        access: FileTime,
        write: FileTime,
        volume: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(
            handle: *mut c_void,
            information: *mut FileInformation,
        ) -> i32;
    }
    let mut information = std::mem::MaybeUninit::<FileInformation>::uninit();
    // SAFETY: the file handle remains valid for the call and the output points
    // to correctly sized writable storage initialized by Kernel32 on success.
    let success = unsafe {
        GetFileInformationByHandle(file.as_raw_handle().cast(), information.as_mut_ptr())
    };
    success != 0 && unsafe { information.assume_init() }.links == 1
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
) -> Result<RegistrySeed, AppError> {
    let content = registry_content_ref(repository, digest);
    let lease = OutputReservation::wait_identity(
        &format!("local-registry-content-tag:{content}"),
        Path::new(content.as_str()),
        "local registry content tag",
        cancellation,
    )?;
    let source_id = inspect_local_image_id(source, cancellation)?.ok_or_else(|| {
        AppError::Environment(format!("registry seed source image is absent: {source}"))
    })?;
    let existing = inspect_local_image_id(&content, cancellation)?;
    let owned_local_tag = local_seed_tag_is_owned(&source_id, existing.as_deref(), &content)?;
    let run = |args: Vec<&str>| -> Result<(), AppError> {
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
        Ok(())
    };
    if owned_local_tag
        && let Err(primary) = run(vec!["image", "tag", source.as_str(), content.as_str()])
    {
        let cleanup = remove_local_registry_tag_after_cancellation(&content);
        return Err(match cleanup {
            Ok(()) => primary,
            Err(cleanup) => AppError::Environment(format!(
                "{primary}; reconcile ambiguous registry seed tag {content}: {cleanup}"
            )),
        });
    }
    if let Err(primary) = run(vec!["push", content.as_str()]) {
        if owned_local_tag {
            let cleanup = remove_local_registry_tag_after_cancellation(&content);
            return Err(match cleanup {
                Ok(()) => primary,
                Err(cleanup) => AppError::Environment(format!(
                    "{primary}; remove failed registry seed tag {content}: {cleanup}"
                )),
            });
        }
        return Err(AppError::Environment(format!(
            "{primary}; pre-existing shared local content tag {content} was safely retained"
        )));
    }
    Ok(RegistrySeed {
        reference: content,
        owned_local_tag,
        _lease: lease,
    })
}

struct RegistrySeed {
    reference: ImageRef,
    owned_local_tag: bool,
    _lease: OutputReservation,
}

fn local_seed_tag_is_owned(
    source_id: &str,
    existing_id: Option<&str>,
    content: &ImageRef,
) -> Result<bool, AppError> {
    match existing_id {
        None => Ok(true),
        Some(existing) if existing == source_id => Ok(false),
        Some(_) => Err(AppError::Environment(format!(
            "refused to replace pre-existing local registry content tag {content}"
        ))),
    }
}

fn inspect_local_image_id(
    reference: &ImageRef,
    cancellation: &dyn Cancellation,
) -> Result<Option<String>, AppError> {
    let invocation = ProcessInvocation {
        program: "docker".into(),
        args: vec![
            "image".into(),
            "inspect".into(),
            "--format".into(),
            "{{.Id}}".into(),
            reference.to_string(),
        ],
        current_dir: None,
    };
    let output = SystemProcessExecutor
        .execute(&invocation, cancellation)
        .map_err(environment("inspect local registry seed tag"))?;
    if output.exit_code == Some(0) {
        let id = output.stdout.trim();
        if id.is_empty() {
            Err(AppError::Environment(format!(
                "inspect local registry seed tag returned no image ID for {reference}"
            )))
        } else {
            Ok(Some(id.to_owned()))
        }
    } else if docker_object_absent(&output.stderr) {
        Ok(None)
    } else {
        Err(AppError::Environment(format!(
            "inspect local registry seed tag: {}",
            output.stderr.trim()
        )))
    }
}

fn registry_content_ref(
    repository: &RegistryRepository,
    digest: &repo_sandbox_core::build::ImageDigest,
) -> ImageRef {
    repository.tagged(&RegistryTag::for_digest(digest))
}

fn seeded_publication(seed: &(ImageRef, BuiltImage)) -> PublishedImage {
    PublishedImage {
        immutable: seed.0.clone(),
        aliases: Vec::new(),
        digest: seed.1.digest.clone(),
        platform_digests: seed.1.platform_digests.clone(),
    }
}

fn remove_local_registry_tag(
    reference: &ImageRef,
    cancellation: &dyn Cancellation,
) -> Result<(), AppError> {
    remove_local_registry_tag_with(&SystemProcessExecutor, reference, cancellation)
}

fn remove_local_registry_tag_after_cancellation(reference: &ImageRef) -> Result<(), AppError> {
    remove_local_registry_tag_after_cancellation_with(&SystemDockerExecutor, reference)
}

fn remove_local_registry_tag_after_cancellation_with(
    executor: &impl DockerExecutor,
    reference: &ImageRef,
) -> Result<(), AppError> {
    let invocation = ProcessInvocation {
        program: "docker".into(),
        args: vec!["image".into(), "rm".into(), reference.to_string()],
        current_dir: None,
    };
    let output = executor
        .execute_cleanup(&invocation, std::time::Duration::from_secs(30))
        .map_err(environment("remove failed registry seed tag"))?;
    if output.exit_code == Some(0) || docker_object_absent(&output.stderr) {
        Ok(())
    } else {
        Err(AppError::Environment(format!(
            "remove failed registry seed tag: {}",
            output.stderr.trim()
        )))
    }
}

fn apply_publication_cleanup(
    report: &mut repo_sandbox_core::runner::RunReport,
    cleanup: Result<(), AppError>,
) -> Option<AppError> {
    cleanup.err().inspect(|error| {
        let message = error.to_string();
        report.cleanup = repo_sandbox_core::runner::CleanupResult::Failed;
        report.cleanup_error = Some(match report.cleanup_error.take() {
            Some(existing) => format!("{existing}; {message}"),
            None => message,
        });
    })
}

fn cleanup_seed_after_publication_failure(
    report: &mut repo_sandbox_core::runner::RunReport,
    seed: Option<(&ImageRef, bool)>,
    primary: AppError,
) -> AppError {
    cleanup_seed_after_publication_failure_with(report, seed, primary, |reference| {
        remove_local_registry_tag_after_cancellation(reference)
    })
}

fn cleanup_seed_after_publication_failure_with(
    report: &mut repo_sandbox_core::runner::RunReport,
    seed: Option<(&ImageRef, bool)>,
    primary: AppError,
    cleanup: impl FnOnce(&ImageRef) -> Result<(), AppError>,
) -> AppError {
    let Some((reference, true)) = seed else {
        return primary;
    };
    match cleanup(reference) {
        Ok(()) => primary,
        Err(error) => {
            let message = error.to_string();
            let _ = apply_publication_cleanup(report, Err(error));
            AppError::Environment(format!("{primary}; local seed cleanup failed: {message}"))
        }
    }
}

fn remove_local_registry_tag_with(
    executor: &impl ProcessExecutor,
    reference: &ImageRef,
    cancellation: &dyn Cancellation,
) -> Result<(), AppError> {
    let invocation = ProcessInvocation {
        program: "docker".into(),
        args: vec!["image".into(), "rm".into(), reference.to_string()],
        current_dir: None,
    };
    let output = executor
        .execute(&invocation, cancellation)
        .map_err(environment("remove local registry content tag"))?;
    if output.exit_code == Some(0) {
        Ok(())
    } else if docker_object_absent(&output.stderr) {
        // A concurrent prune/untag has already achieved the cleanup outcome.
        Ok(())
    } else {
        Err(AppError::Environment(format!(
            "remove local registry content tag: {}",
            output.stderr.trim()
        )))
    }
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

    struct FixedExecutor(crate::buildkit::ProcessOutput);

    struct CleanupExecutor {
        calls: std::sync::Mutex<Vec<ProcessInvocation>>,
    }

    struct SequenceExecutor {
        calls: std::sync::Mutex<Vec<ProcessInvocation>>,
        outputs: std::sync::Mutex<std::collections::VecDeque<crate::buildkit::ProcessOutput>>,
    }

    struct FallibleSequenceExecutor {
        calls: std::sync::Mutex<Vec<ProcessInvocation>>,
        outputs: std::sync::Mutex<
            std::collections::VecDeque<std::io::Result<crate::buildkit::ProcessOutput>>,
        >,
    }

    impl ProcessExecutor for SequenceExecutor {
        fn execute(
            &self,
            invocation: &ProcessInvocation,
            _cancellation: &dyn Cancellation,
        ) -> std::io::Result<crate::buildkit::ProcessOutput> {
            self.calls.lock().unwrap().push(invocation.clone());
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| std::io::Error::other("missing sequence output"))
        }
    }

    impl ProcessExecutor for FallibleSequenceExecutor {
        fn execute(
            &self,
            invocation: &ProcessInvocation,
            _cancellation: &dyn Cancellation,
        ) -> std::io::Result<crate::buildkit::ProcessOutput> {
            self.calls.lock().unwrap().push(invocation.clone());
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(std::io::Error::other("missing sequence output")))
        }
    }

    impl DockerExecutor for CleanupExecutor {
        fn execute(
            &self,
            _invocation: &ProcessInvocation,
            _timeout: std::time::Duration,
        ) -> std::io::Result<crate::buildkit::ProcessOutput> {
            panic!("failed seed cleanup must not use cancellation-aware execution")
        }

        fn execute_cleanup(
            &self,
            invocation: &ProcessInvocation,
            _timeout: std::time::Duration,
        ) -> std::io::Result<crate::buildkit::ProcessOutput> {
            self.calls.lock().unwrap().push(invocation.clone());
            Ok(crate::buildkit::ProcessOutput {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                interrupted: false,
            })
        }
    }

    impl ProcessExecutor for FixedExecutor {
        fn execute(
            &self,
            _invocation: &ProcessInvocation,
            _cancellation: &dyn Cancellation,
        ) -> std::io::Result<crate::buildkit::ProcessOutput> {
            Ok(self.0.clone())
        }
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
            &NeverCancelled,
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
    fn oci_publication_never_replaces_a_late_noncooperating_destination() {
        let temporary = tempfile::tempdir().unwrap();
        let amd64 = temporary.path().join("amd64");
        fake_layout(&amd64, &"a".repeat(64));
        let output = temporary.path().join("result");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("sentinel"), "unchanged").unwrap();
        let staging = temporary.path().join("staging");
        fs::create_dir(&staging).unwrap();
        assert!(
            merge_oci_layouts(
                &[(Platform::LinuxAmd64, amd64)],
                &output,
                &staging,
                &NeverCancelled,
            )
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(output.join("sentinel")).unwrap(),
            "unchanged"
        );
    }

    #[test]
    fn oci_blob_copy_observes_cancellation_and_never_publishes_partial_layout() {
        struct CancelDuringBlob(std::sync::atomic::AtomicUsize);
        impl Cancellation for CancelDuringBlob {
            fn is_cancelled(&self) -> bool {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 5
            }
        }
        let source_root = tempfile::tempdir().unwrap();
        let layout = source_root.path().join("amd64");
        fake_layout(&layout, &"a".repeat(64));
        fs::write(
            layout.join("blobs/sha256").join("a".repeat(64)),
            vec![b'x'; 256 * 1024],
        )
        .unwrap();
        let destination_root = tempfile::tempdir().unwrap();
        let output = destination_root.path().join("layout");
        let staging = tempfile::tempdir().unwrap();
        let staging_path = staging.path().to_path_buf();
        let error = merge_oci_layouts(
            &[(Platform::LinuxAmd64, layout)],
            &output,
            staging.path(),
            &CancelDuringBlob(std::sync::atomic::AtomicUsize::new(0)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("OCI publication cancelled"));
        assert!(!output.exists());
        drop(staging);
        assert!(!staging_path.exists());
    }

    #[test]
    fn oci_cancellation_at_commit_boundary_never_publishes_destination() {
        struct Flag(std::sync::atomic::AtomicBool);
        impl Cancellation for Flag {
            fn is_cancelled(&self) -> bool {
                self.0.load(std::sync::atomic::Ordering::Acquire)
            }
        }
        let source_root = tempfile::tempdir().unwrap();
        let layout = source_root.path().join("amd64");
        fake_layout(&layout, &"a".repeat(64));
        let destination_root = tempfile::tempdir().unwrap();
        let output = destination_root.path().join("layout");
        let staging = tempfile::tempdir().unwrap();
        let cancellation = Flag(std::sync::atomic::AtomicBool::new(false));
        let error = merge_oci_layouts_with_hook(
            &[(Platform::LinuxAmd64, layout)],
            &output,
            staging.path(),
            &cancellation,
            || {
                cancellation
                    .0
                    .store(true, std::sync::atomic::Ordering::Release)
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("OCI publication cancelled"));
        assert!(!output.exists());
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
    fn oci_guard_prepares_and_binds_a_fresh_nested_output_parent() {
        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path().join("reports/task-oci");
        let guard = ExternalOciGuard::prepare(&parent.join("layout")).unwrap();
        let staging = create_oci_staging(&guard.bound_path().unwrap()).unwrap();
        assert!(parent.is_dir());
        #[cfg(windows)]
        assert_eq!(
            staging.path().parent().unwrap().canonicalize().unwrap(),
            parent.canonicalize().unwrap()
        );
        #[cfg(unix)]
        assert_eq!(
            state_identity_from_handle(guard.handle.as_ref().unwrap()).unwrap(),
            state_identity(&parent).unwrap()
        );
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
    fn cache_rotation_cannot_replace_an_export_while_it_is_being_imported() {
        let temporary = tempfile::tempdir().unwrap();
        let cancellation = DeadlineCancellation::new(std::time::Duration::from_secs(1));
        let import = CacheLease::shared(temporary.path(), &cancellation).unwrap();
        let contender = WorkflowLease::open(&temporary.path().join(".rotation.lock")).unwrap();
        assert!(matches!(
            contender.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(import);
        contender.try_lock().unwrap();
    }

    #[test]
    fn failed_task_cache_cleanup_removes_only_its_unique_export() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let failed = task_cache_export(&cache, "failed-task");
        let sibling = task_cache_export(&cache, "concurrent-task");
        fs::create_dir_all(&failed).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        fs::write(failed.join("partial"), "partial").unwrap();
        fs::write(sibling.join("complete"), "complete").unwrap();
        let primary = AppError::Environment("environment image: interrupted".into());
        let returned = clean_failed_cache_export(&failed, primary);
        assert!(
            returned
                .to_string()
                .contains("environment image: interrupted")
        );
        assert!(!failed.exists());
        assert_eq!(
            fs::read_to_string(sibling.join("complete")).unwrap(),
            "complete"
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
    fn multi_platform_staging_index_is_unique_per_source_snapshot() {
        let repository = RegistryRepository::new("registry.test/team/task").unwrap();
        let plan = format!("sha256:{}", "a".repeat(64));
        let first = multi_platform_index_ref(
            &repository,
            &plan,
            &"b".repeat(64),
            &[format!("registry.test/task@sha256:{}", "d".repeat(64))],
        )
        .unwrap();
        let second = multi_platform_index_ref(
            &repository,
            &plan,
            &"c".repeat(64),
            &[format!("registry.test/task@sha256:{}", "d".repeat(64))],
        )
        .unwrap();
        assert_ne!(first, second);
        assert!(first.as_str().starts_with("registry.test/team/task:multi-"));
        let changed_environment = multi_platform_index_ref(
            &repository,
            &plan,
            &"b".repeat(64),
            &[format!("registry.test/task@sha256:{}", "e".repeat(64))],
        )
        .unwrap();
        assert_ne!(first, changed_environment);
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

    #[cfg(unix)]
    #[test]
    fn state_swap_after_validation_is_rejected_at_the_write_boundary() {
        use std::os::unix::fs::symlink;
        let repository = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, "unchanged").unwrap();
        let state = repository.path().join(".repo-sandbox");
        prepare_state_layout(repository.path(), &state).unwrap();
        let cache = state.join("cache");
        let saved = state.join("cache-saved");
        let result = prepare_state_layout_with_hook(repository.path(), &state, || {
            fs::rename(&cache, &saved).unwrap();
            symlink(outside.path(), &cache).unwrap();
        });
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged");
        fs::remove_file(cache).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bound_directory_fd_closes_the_final_check_to_write_swap_window() {
        use std::os::unix::fs::symlink;
        let repository = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, "unchanged").unwrap();
        let state = repository.path().join(".repo-sandbox");
        let guard = prepare_state_layout(repository.path(), &state).unwrap();
        let cache = state.join("cache");
        let bound_marker = guard.bound_path(&cache.join(OWNER_MARKER)).unwrap();
        let saved = state.join("cache-saved");
        // The attacker swaps the pathname after the last validation. The write is
        // still relative to the already-open cache fd, never the replacement link.
        fs::rename(&cache, &saved).unwrap();
        symlink(outside.path(), &cache).unwrap();
        write_state_file(&bound_marker, b"owned").unwrap();
        assert_eq!(
            fs::read_to_string(saved.join(OWNER_MARKER)).unwrap(),
            "owned"
        );
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged");
        fs::remove_file(cache).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn state_capture_detects_replacement_after_safe_open() {
        use std::os::unix::fs::symlink;
        let repository = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, "unchanged").unwrap();
        let state = repository.path().join(".repo-sandbox");
        fs::create_dir(&state).unwrap();
        let saved = repository.path().join("state-saved");
        let result = StateLayoutGuard::capture_with_hook(
            repository.path(),
            std::slice::from_ref(&state),
            |_| {
                fs::rename(&state, &saved).unwrap();
                symlink(outside.path(), &state).unwrap();
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged");
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
        fs::remove_file(state).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn clean_journal_swap_after_revision_fails_closed() {
        use std::os::unix::fs::symlink;
        let repository = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, "unchanged").unwrap();
        let state = repository.path().join(".repo-sandbox");
        let guard = prepare_state_layout(repository.path(), &state).unwrap();
        drop(guard);
        let tasks = state.join("tasks");
        let missing = repository.path().join("missing-source");
        let candidate = CleanCandidate {
            task_id: "task".into(),
            repository_id: "repository".into(),
            kind: ResourceKind::Source,
            identifier: missing.display().to_string(),
            owner: "task".into(),
            state: ResourceState::Registered,
        };
        let plan = CleanPlan {
            candidates: vec![candidate],
            manifest_root: Some(tasks.clone()),
            journal_roots: [("repository".into(), tasks.clone())].into_iter().collect(),
            lease_path: Some(state.join(".workflow.lock")),
            journal_revisions: [(tasks.clone(), Vec::new())].into_iter().collect(),
            refused: Vec::new(),
        };
        let saved = state.join("tasks-saved");
        let result = execute_clean_with_hook(&plan, false, &NeverCancelled, || {
            fs::rename(&tasks, &saved).unwrap();
            symlink(outside.path(), &tasks).unwrap();
        })
        .unwrap();
        assert!(!result.complete());
        assert_eq!(result.absent, vec![missing.display().to_string()]);
        assert_eq!(result.failed.len(), 1);
        assert!(result.failed[0].contains("workflow state component must be a real directory"));
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged");
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
        fs::remove_file(tasks).unwrap();
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

    #[cfg(windows)]
    #[test]
    fn junction_swap_after_validation_is_rejected_at_the_write_boundary() {
        let repository = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, "unchanged").unwrap();
        let state = repository.path().join(".repo-sandbox");
        prepare_state_layout(repository.path(), &state).unwrap();
        let cache = state.join("cache");
        let saved = state.join("cache-saved");
        let result = prepare_state_layout_with_hook(repository.path(), &state, || {
            fs::rename(&cache, &saved).unwrap();
            assert!(
                std::process::Command::new("cmd")
                    .args([
                        "/c",
                        "mklink",
                        "/J",
                        &cache.to_string_lossy(),
                        &outside.path().to_string_lossy(),
                    ])
                    .status()
                    .unwrap()
                    .success()
            );
        });
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged");
        fs::remove_dir(cache).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn bound_directory_handle_prevents_post_validation_replacement() {
        let repository = tempfile::tempdir().unwrap();
        let state = repository.path().join(".repo-sandbox");
        let guard = prepare_state_layout(repository.path(), &state).unwrap();
        let cache = state.join("cache");
        assert!(fs::rename(&cache, state.join("cache-replaced")).is_err());
        write_state_file(
            &guard.bound_path(&cache.join(OWNER_MARKER)).unwrap(),
            b"owned",
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(cache.join(OWNER_MARKER)).unwrap(),
            "owned"
        );
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

    #[cfg(unix)]
    #[test]
    fn output_reservation_root_is_private_to_the_effective_user() {
        use std::os::unix::fs::MetadataExt;
        let root = output_reservation_root().unwrap();
        let metadata = fs::symlink_metadata(root).unwrap();
        assert_eq!(metadata.uid(), effective_uid());
        assert_eq!(metadata.mode() & 0o077, 0);
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

    fn assert_aliased_output_reservations_conflict(real: &Path, alias: &Path) {
        let _reservation = OutputReservation::report(&real.join("output")).unwrap();
        for kind in ["report", "oci"] {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "workflow::tests::aliased_output_reservation_process_helper",
                    "--nocapture",
                ])
                .env(
                    "REPO_SANDBOX_ALIAS_RESERVATION_OUTPUT",
                    alias.join("output"),
                )
                .env("REPO_SANDBOX_ALIAS_RESERVATION_KIND", kind)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "{kind} alias acquired a distinct reservation"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_aliased_output_reservations_share_one_cross_process_lock() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = root.path().join("alias");
        symlink(&real, &alias).unwrap();
        assert_aliased_output_reservations_conflict(&real, &alias);
    }

    #[cfg(windows)]
    #[test]
    fn junction_aliased_output_reservations_share_one_cross_process_lock() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = root.path().join("alias");
        assert!(
            std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    &alias.to_string_lossy(),
                    &real.to_string_lossy()
                ])
                .status()
                .unwrap()
                .success()
        );
        assert_aliased_output_reservations_conflict(&real, &alias);
        fs::remove_dir(alias).unwrap();
    }

    #[test]
    fn aliased_output_reservation_process_helper() {
        let Some(output) = std::env::var_os("REPO_SANDBOX_ALIAS_RESERVATION_OUTPUT") else {
            return;
        };
        let result = if std::env::var("REPO_SANDBOX_ALIAS_RESERVATION_KIND").as_deref() == Ok("oci")
        {
            OutputReservation::oci(Path::new(&output))
        } else {
            OutputReservation::report(Path::new(&output))
        };
        assert!(result.is_err());
    }

    #[test]
    fn registry_preflight_uses_the_single_platform_push_boundary_and_records_remote_fact() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let success = |stdout: &str| crate::buildkit::ProcessOutput {
            exit_code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
            interrupted: false,
        };
        let executor = SequenceExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
            outputs: std::sync::Mutex::new(
                [
                    success(""),
                    success(&format!("digest: {digest}")),
                    success(&format!("Digest: {digest}")),
                    success(&format!(
                        "sha256:{}|registry-preflight|fixture",
                        "d".repeat(64)
                    )),
                    success(""),
                ]
                .into(),
            ),
        };
        let deadline = DeadlineCancellation::new(std::time::Duration::from_secs(2));
        let mut facts = Vec::new();
        preflight_registry_with(
            &executor,
            "localhost:5000/team/image",
            "fixture",
            false,
            &deadline,
            |fact| facts.push(fact),
        )
        .unwrap();
        let calls = executor.calls.lock().unwrap();
        assert_eq!(calls.len(), 5);
        assert_eq!(calls[0].args[0], "build");
        assert_eq!(calls[1].args[0], "push");
        assert_eq!(&calls[2].args[..3], ["buildx", "imagetools", "inspect"]);
        assert_eq!(&calls[3].args[..2], ["image", "inspect"]);
        assert_eq!(&calls[4].args[..3], ["image", "rm", "--force"]);
        assert_eq!(facts.len(), 2);
        assert!(!facts[0].verified);
        assert!(facts[1].verified);
        assert_eq!(facts[1].kind, PublicationFactKind::RegistryPreflightStaging);
    }

    #[test]
    fn registry_preflight_uses_the_multi_platform_buildx_push_boundary() {
        let digest = format!("sha256:{}", "b".repeat(64));
        let success = |stdout: &str| crate::buildkit::ProcessOutput {
            exit_code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
            interrupted: false,
        };
        let executor = SequenceExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
            outputs: std::sync::Mutex::new(
                [
                    success(&format!("pushing manifest@{digest}")),
                    success(&format!("Digest: {digest}")),
                ]
                .into(),
            ),
        };
        let deadline = DeadlineCancellation::new(std::time::Duration::from_secs(2));
        let mut facts = Vec::new();
        preflight_registry_with(
            &executor,
            "localhost:5000/team/image",
            "fixture",
            true,
            &deadline,
            |fact| facts.push(fact),
        )
        .unwrap();
        let calls = executor.calls.lock().unwrap();
        assert_eq!(&calls[0].args[..2], ["buildx", "build"]);
        assert!(
            calls[0]
                .args
                .iter()
                .any(|arg| arg.contains("type=image") && arg.contains("push=true"))
        );
        assert_eq!(&calls[1].args[..3], ["buildx", "imagetools", "inspect"]);
        assert_eq!(facts.last().unwrap().digest.as_str(), digest);
    }

    #[test]
    fn registry_preflight_retains_the_remote_fact_when_verification_fails() {
        let digest = format!("sha256:{}", "c".repeat(64));
        let executor = SequenceExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
            outputs: std::sync::Mutex::new(
                [
                    crate::buildkit::ProcessOutput {
                        exit_code: Some(0),
                        stdout: format!("pushed@{digest}"),
                        stderr: String::new(),
                        interrupted: false,
                    },
                    crate::buildkit::ProcessOutput {
                        exit_code: Some(1),
                        stdout: String::new(),
                        stderr: "verification unavailable".into(),
                        interrupted: false,
                    },
                ]
                .into(),
            ),
        };
        let deadline = DeadlineCancellation::new(std::time::Duration::from_secs(2));
        let mut facts = Vec::new();
        let error = preflight_registry_with(
            &executor,
            "localhost:5000/team/image",
            "fixture",
            true,
            &deadline,
            |fact| facts.push(fact),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("reconciliation did not stabilize")
        );
        assert_eq!(facts.len(), 1);
        assert!(!facts[0].verified);
        assert_eq!(
            facts[0].reference.as_str(),
            "localhost:5000/team/image:preflight-fixture"
        );
    }

    #[test]
    fn registry_preflight_reconciles_a_committed_manifest_after_nonzero_push() {
        let digest = format!("sha256:{}", "e".repeat(64));
        let executor = SequenceExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
            outputs: std::sync::Mutex::new(
                [
                    crate::buildkit::ProcessOutput {
                        exit_code: Some(1),
                        stdout: String::new(),
                        stderr: "connection closed after upload".into(),
                        interrupted: false,
                    },
                    crate::buildkit::ProcessOutput {
                        exit_code: Some(0),
                        stdout: format!("Digest: {digest}"),
                        stderr: String::new(),
                        interrupted: false,
                    },
                ]
                .into(),
            ),
        };
        let deadline = DeadlineCancellation::new(std::time::Duration::from_secs(2));
        let mut facts = Vec::new();
        let error = preflight_registry_with(
            &executor,
            "localhost:5000/team/image",
            "fixture",
            true,
            &deadline,
            |fact| facts.push(fact),
        )
        .unwrap_err();
        assert!(error.to_string().contains("push registry preflight image"));
        assert_eq!(facts.len(), 1);
        assert!(facts[0].verified);
        assert_eq!(facts[0].digest.as_str(), digest);
    }

    #[test]
    fn failed_single_registry_probe_still_removes_only_its_exact_owned_image_id() {
        let image_id = format!("sha256:{}", "f".repeat(64));
        let output = |code, stdout: &str, stderr: &str| crate::buildkit::ProcessOutput {
            exit_code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
            interrupted: false,
        };
        let executor = SequenceExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
            outputs: std::sync::Mutex::new(
                [
                    output(0, "", ""),
                    output(1, "", "push failed"),
                    output(1, "", "manifest unknown"),
                    output(1, "", "manifest unknown"),
                    output(1, "", "manifest unknown"),
                    output(0, &format!("{image_id}|registry-preflight|fixture"), ""),
                    output(0, "", ""),
                ]
                .into(),
            ),
        };
        let deadline = DeadlineCancellation::new(std::time::Duration::from_secs(2));
        let error = preflight_registry_with(
            &executor,
            "localhost:5000/team/image",
            "fixture",
            false,
            &deadline,
            |_| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains("push registry preflight image"));
        let calls = executor.calls.lock().unwrap();
        assert_eq!(calls.last().unwrap().args.last(), Some(&image_id));
    }

    #[test]
    fn writable_layer_quota_preflight_uses_an_offline_owned_image_and_cleans_it() {
        let ok = || crate::buildkit::ProcessOutput {
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            interrupted: false,
        };
        let owned = |kind: char| crate::buildkit::ProcessOutput {
            stdout: format!(
                "sha256:{}|quota-probe|fixture\n",
                kind.to_string().repeat(64)
            ),
            ..ok()
        };
        let executor = SequenceExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
            outputs: std::sync::Mutex::new([ok(), owned('a'), ok(), owned('b'), ok(), ok()].into()),
        };
        preflight_writable_layer_quota_with_identity(&executor, 384, &NeverCancelled, "fixture")
            .unwrap();
        let calls = executor.calls.lock().unwrap();
        assert_eq!(calls.len(), 6);
        assert_eq!(calls[0].args[0], "build");
        assert!(calls[0].args.iter().any(|arg| arg.ends_with("Dockerfile")));
        assert_eq!(&calls[1].args[..2], ["image", "inspect"]);
        assert_eq!(&calls[2].args[..2], ["container", "create"]);
        assert!(calls[2].args.contains(&"size=384m".into()));
        assert_eq!(&calls[3].args[..2], ["container", "inspect"]);
        assert_eq!(&calls[4].args[..3], ["container", "rm", "--force"]);
        assert!(calls[4].args[3].starts_with("sha256:"));
        assert_eq!(&calls[5].args[..3], ["image", "rm", "--force"]);
    }

    #[test]
    fn writable_layer_quota_rejection_is_environment_failure_and_still_cleans() {
        let output = |code, stderr: &str| crate::buildkit::ProcessOutput {
            exit_code: Some(code),
            stdout: String::new(),
            stderr: stderr.into(),
            interrupted: false,
        };
        let executor = SequenceExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
            outputs: std::sync::Mutex::new(
                [
                    output(0, ""),
                    crate::buildkit::ProcessOutput {
                        stdout: format!("sha256:{}|quota-probe|fixture\n", "a".repeat(64)),
                        ..output(0, "")
                    },
                    output(1, "storage quota unsupported"),
                    output(1, "No such container"),
                    output(1, "No such container"),
                    output(1, "No such container"),
                    output(0, ""),
                ]
                .into(),
            ),
        };
        let error = preflight_writable_layer_quota_with_identity(
            &executor,
            512,
            &NeverCancelled,
            "fixture",
        )
        .unwrap_err();
        assert_eq!(error.exit_code().as_i32(), 3);
        assert!(error.to_string().contains("writable-layer quota"));
        assert_eq!(executor.calls.lock().unwrap().len(), 7);
    }

    #[test]
    fn writable_layer_quota_never_removes_a_foreign_same_name_container() {
        let output = |code, stdout: &str, stderr: &str| crate::buildkit::ProcessOutput {
            exit_code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
            interrupted: false,
        };
        let image_id = format!("sha256:{}", "a".repeat(64));
        let foreign_id = format!("sha256:{}", "b".repeat(64));
        let executor = SequenceExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
            outputs: std::sync::Mutex::new(
                [
                    output(0, "", ""),
                    output(0, &format!("{image_id}|quota-probe|fixture\n"), ""),
                    output(1, "", "name already in use"),
                    output(0, &format!("{foreign_id}|quota-probe|foreign\n"), ""),
                    output(0, "", ""),
                ]
                .into(),
            ),
        };
        let error = preflight_writable_layer_quota_with_identity(
            &executor,
            512,
            &NeverCancelled,
            "fixture",
        )
        .unwrap_err();
        assert!(error.to_string().contains("foreign quota probe container"));
        let calls = executor.calls.lock().unwrap();
        assert!(!calls.iter().any(|call| {
            call.args
                .starts_with(&["container".into(), "rm".into(), "--force".into()])
        }));
        assert_eq!(calls.last().unwrap().args.last(), Some(&image_id));
    }

    #[test]
    fn writable_layer_quota_reconciles_an_owned_container_created_after_interruption() {
        let output = |code, stdout: &str, stderr: &str| crate::buildkit::ProcessOutput {
            exit_code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
            interrupted: false,
        };
        let image_id = format!("sha256:{}", "a".repeat(64));
        let container_id = format!("sha256:{}", "b".repeat(64));
        let executor = SequenceExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
            outputs: std::sync::Mutex::new(
                [
                    output(0, "", ""),
                    output(0, &format!("{image_id}|quota-probe|fixture\n"), ""),
                    crate::buildkit::ProcessOutput {
                        interrupted: true,
                        ..output(1, "", "interrupted")
                    },
                    output(1, "", "No such container"),
                    output(0, &format!("{container_id}|quota-probe|fixture\n"), ""),
                    output(0, "", ""),
                    output(0, "", ""),
                ]
                .into(),
            ),
        };
        let error = preflight_writable_layer_quota_with_identity(
            &executor,
            512,
            &NeverCancelled,
            "fixture",
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled or timed out"));
        let calls = executor.calls.lock().unwrap();
        assert_eq!(calls[calls.len() - 2].args.last(), Some(&container_id));
        assert_eq!(calls[calls.len() - 1].args.last(), Some(&image_id));
    }

    #[test]
    fn writable_layer_quota_reconciles_and_cleans_after_build_executor_io_error() {
        let output = |code, stdout: &str, stderr: &str| crate::buildkit::ProcessOutput {
            exit_code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
            interrupted: false,
        };
        let image_id = format!("sha256:{}", "a".repeat(64));
        let executor = FallibleSequenceExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
            outputs: std::sync::Mutex::new(
                [
                    Err(std::io::Error::other("I/O failed after spawn")),
                    Ok(output(0, &format!("{image_id}|quota-probe|fixture"), "")),
                    Ok(output(1, "", "No such container")),
                    Ok(output(1, "", "No such container")),
                    Ok(output(1, "", "No such container")),
                    Ok(output(0, "", "")),
                ]
                .into(),
            ),
        };
        let error = preflight_writable_layer_quota_with_identity(
            &executor,
            512,
            &NeverCancelled,
            "fixture",
        )
        .unwrap_err();
        assert!(error.to_string().contains("I/O failed after spawn"));
        assert_eq!(
            executor.calls.lock().unwrap().last().unwrap().args.last(),
            Some(&image_id)
        );
    }

    #[test]
    fn writable_layer_quota_reconciles_and_cleans_after_create_executor_io_error() {
        let output = |code, stdout: &str| crate::buildkit::ProcessOutput {
            exit_code: Some(code),
            stdout: stdout.into(),
            stderr: String::new(),
            interrupted: false,
        };
        let image_id = format!("sha256:{}", "a".repeat(64));
        let container_id = format!("sha256:{}", "b".repeat(64));
        let executor = FallibleSequenceExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
            outputs: std::sync::Mutex::new(
                [
                    Ok(output(0, "")),
                    Ok(output(0, &format!("{image_id}|quota-probe|fixture"))),
                    Err(std::io::Error::other("create I/O failed after spawn")),
                    Ok(output(0, &format!("{container_id}|quota-probe|fixture"))),
                    Ok(output(0, "")),
                    Ok(output(0, "")),
                ]
                .into(),
            ),
        };
        let error = preflight_writable_layer_quota_with_identity(
            &executor,
            512,
            &NeverCancelled,
            "fixture",
        )
        .unwrap_err();
        assert!(error.to_string().contains("create I/O failed after spawn"));
        let calls = executor.calls.lock().unwrap();
        assert_eq!(calls[calls.len() - 2].args.last(), Some(&container_id));
        assert_eq!(calls[calls.len() - 1].args.last(), Some(&image_id));
    }

    #[cfg(not(windows))]
    #[test]
    fn disk_preflight_propagates_cancellation_from_its_subprocess() {
        let executor = FixedExecutor(crate::buildkit::ProcessOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            interrupted: true,
        });
        let error =
            workflow_available_space_with(&executor, Path::new("."), &NeverCancelled).unwrap_err();
        assert!(error.to_string().contains("disk preflight was cancelled"));
        assert_eq!(
            parse_df_available_space(
                "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/test 100 20 80 20% /"
            )
            .unwrap(),
            80 * 1024
        );
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

    #[test]
    fn seeded_publication_records_the_irreversible_remote_fact_without_aliases() {
        let digest =
            repo_sandbox_core::build::ImageDigest::new(format!("sha256:{}", "a".repeat(64)))
                .unwrap();
        let seed = (
            ImageRef::new("registry.test/team/task:sha256-content").unwrap(),
            BuiltImage {
                image: ImageRef::new("repo-sandbox-task:local").unwrap(),
                digest: digest.clone(),
                platform_digests: Vec::new(),
            },
        );
        let publication = seeded_publication(&seed);
        assert_eq!(publication.immutable, seed.0);
        assert_eq!(publication.digest, digest);
        assert!(publication.aliases.is_empty());
    }

    #[test]
    fn successful_single_publication_removes_its_local_content_tag() {
        let executor = InspectExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
            image_id: String::new(),
        };
        let reference = ImageRef::new("registry.test/team/task:sha256-content").unwrap();
        remove_local_registry_tag_with(&executor, &reference, &NeverCancelled).unwrap();
        let calls = executor.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].args,
            ["image", "rm", reference.as_str()].map(str::to_owned)
        );
    }

    #[test]
    fn missing_local_publication_tag_is_an_idempotent_cleanup_success() {
        let executor = FixedExecutor(crate::buildkit::ProcessOutput {
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "Error: No such image: registry.test/team/task:content".into(),
            interrupted: false,
        });
        let reference = ImageRef::new("registry.test/team/task:content").unwrap();
        remove_local_registry_tag_with(&executor, &reference, &NeverCancelled).unwrap();
    }

    #[test]
    fn failed_seed_cleanup_uses_the_post_cancellation_bounded_path() {
        let executor = CleanupExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
        };
        let reference = ImageRef::new("registry.test/team/task:sha256-content").unwrap();
        remove_local_registry_tag_after_cancellation_with(&executor, &reference).unwrap();
        let calls = executor.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].args,
            ["image", "rm", reference.as_str()].map(str::to_owned)
        );
    }

    #[test]
    fn preexisting_shared_seed_tag_is_never_claimed_or_removed() {
        let content = ImageRef::new("registry.test/team/task:sha256-content").unwrap();
        assert!(!local_seed_tag_is_owned("sha256:same", Some("sha256:same"), &content).unwrap());
        assert!(local_seed_tag_is_owned("sha256:new", None, &content).unwrap());
        assert!(
            local_seed_tag_is_owned("sha256:expected", Some("sha256:foreign"), &content)
                .unwrap_err()
                .to_string()
                .contains("refused to replace")
        );
    }

    #[test]
    fn identical_concurrent_seed_tags_share_one_exclusive_lease() {
        let identity = "local-registry-content-tag:registry.test/team/task:sha256-content";
        let display = Path::new("registry.test/team/task:sha256-content");
        let first = OutputReservation::create_identity(identity, display, "seed").unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            let lease = OutputReservation::wait_identity(
                identity,
                display,
                "seed",
                &DeadlineCancellation::new(std::time::Duration::from_secs(2)),
            )
            .unwrap();
            sender.send(()).unwrap();
            lease
        });
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
        );
        drop(first);
        receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        drop(second.join().unwrap());
    }

    #[test]
    fn unregistered_shared_task_image_is_retained_instead_of_force_removed() {
        let image_id = ImageRef::new(format!("sha256:{}", "d".repeat(64))).unwrap();
        let error = registration_failure_with_safe_retention(
            AppError::Environment("journal full".into()),
            &image_id,
        );
        assert!(error.to_string().contains("journal full"));
        assert!(error.to_string().contains("safely retained"));
        assert!(error.to_string().contains(image_id.as_str()));
    }

    #[test]
    fn failed_source_registration_keeps_automatic_deletion_armed() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("tracked.txt"), "source").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(source.path())
                .status()
                .unwrap()
                .success()
        );
        let mut materialized = GitSnapshotter::default()
            .create(
                &SourceSpec::LocalDirectory(source.path().to_path_buf()),
                SnapshotOptions {
                    recurse_submodules: false,
                    cleanup: CleanupPolicy::Delete,
                },
            )
            .unwrap();
        let materialized_path = materialized.path().to_path_buf();
        let error = retain_source_after_registration(&mut materialized, |_| {
            Err(AppError::Environment("journal unavailable".into()))
        })
        .unwrap_err();
        assert!(error.to_string().contains("journal unavailable"));
        assert!(materialized.is_automatically_cleaned());
        drop(materialized);
        assert!(!materialized_path.exists());
    }

    #[test]
    fn publication_cleanup_failure_preserves_remote_publication_in_report() {
        use repo_sandbox_core::build::ImageDigest;
        use repo_sandbox_core::registry::PublishedImage;
        use repo_sandbox_core::runner::{CleanupResult, RunReport};
        use repo_sandbox_core::snapshot::{SnapshotId, SnapshotOrigin, SourceSnapshot};
        let digest = ImageDigest::new(format!("sha256:{}", "a".repeat(64))).unwrap();
        let publication = PublishedImage {
            immutable: ImageRef::new("registry.test/team/task:sha256-content").unwrap(),
            aliases: vec![ImageRef::new("registry.test/team/task:verified").unwrap()],
            digest: digest.clone(),
            platform_digests: Vec::new(),
        };
        let mut report = RunReport {
            schema_version: 1,
            plan_digest: format!("sha256:{}", "b".repeat(64)),
            phase: "complete".into(),
            exit_code: 0,
            message: "workflow succeeded".into(),
            task_id: "task".into(),
            container_id: None,
            source_snapshot: SourceSnapshot {
                id: SnapshotId::parse("c".repeat(64)).unwrap(),
                origin: SnapshotOrigin::Local {
                    canonical_root: PathBuf::from("repository"),
                },
                file_count: 1,
                recurse_submodules: false,
            },
            config: ConfigSummary {
                template_id: "rust-bazel".into(),
                plan_digest: format!("sha256:{}", "b".repeat(64)),
                platform: Platform::LinuxAmd64,
                build_steps: Vec::new(),
                test_steps: Vec::new(),
                artifact_directories: Vec::new(),
            },
            image: ImageRef::new("sha256:local-image-id").unwrap(),
            image_digest: digest,
            started_at_unix_ms: 1,
            ended_at_unix_ms: 2,
            duration_ms: 1,
            status: RunStatus::Succeeded,
            steps: Vec::new(),
            exported_artifacts: Vec::new(),
            artifact_error: None,
            cleanup: CleanupResult::Removed,
            cleanup_error: None,
            published: Some(publication.clone()),
            publication_progress: Vec::new(),
        };
        let error = AppError::Environment("remove local registry content tag: denied".into());
        let returned = apply_publication_cleanup(&mut report, Err(error)).unwrap();
        annotate_report(&mut report);
        assert_eq!(report.published, Some(publication.clone()));
        assert_eq!(report.cleanup, CleanupResult::Failed);
        assert!(report.cleanup_error.as_deref().unwrap().contains("denied"));
        assert_eq!(report.exit_code, 3);
        assert!(returned.to_string().contains("denied"));
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("sha256-content"));
        assert!(json.contains("verified"));

        let seed = ImageRef::new("registry.test/team/task:sha256-seed").unwrap();
        let called = std::cell::Cell::new(false);
        let primary = cleanup_seed_after_publication_failure_with(
            &mut report,
            Some((&seed, true)),
            AppError::Environment("alias verification failed".into()),
            |observed| {
                assert_eq!(observed, &seed);
                called.set(true);
                Ok(())
            },
        );
        assert!(called.get());
        assert!(primary.to_string().contains("alias verification failed"));
        assert_eq!(report.published, Some(publication));

        report.published = None;
        report.publication_progress = vec![RemotePublicationFact {
            kind: PublicationFactKind::TaskStaging,
            reference: seed,
            digest: report.image_digest.clone(),
            verified: true,
            finality: PublicationFinality::Staging,
        }];
        let partial = serde_json::to_string(&report).unwrap();
        assert!(partial.contains("\"kind\":\"task_staging\""));
        assert!(partial.contains("\"finality\":\"staging\""));
        assert!(!partial.contains("\"immutable\""));
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
                .contains("overlap")
        );
        assert_eq!(
            normalized_output_path(Path::new("reports/other/../result.json")).unwrap(),
            normalized_output_path(Path::new("reports/result.json")).unwrap()
        );
    }

    #[test]
    fn ancestor_output_paths_are_rejected_before_execution() {
        for (report, oci) in [("out", "out/layout"), ("out/report.json", "out")] {
            let mut plan = default_execution_plan();
            plan.request.report = Some(PathBuf::from(report));
            plan.request.oci_layout = Some(PathBuf::from(oci));
            assert!(
                validate_outputs(&plan)
                    .unwrap_err()
                    .to_string()
                    .contains("overlap")
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_aliased_report_and_oci_outputs_are_rejected() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = root.path().join("alias");
        symlink(&real, &alias).unwrap();
        let mut plan = default_execution_plan();
        plan.request.report = Some(real.join("result"));
        plan.request.oci_layout = Some(alias.join("result"));
        assert!(
            validate_outputs(&plan)
                .unwrap_err()
                .to_string()
                .contains("overlap")
        );
    }

    #[cfg(windows)]
    #[test]
    fn junction_aliased_report_and_oci_outputs_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = root.path().join("alias");
        assert!(
            std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    &alias.to_string_lossy(),
                    &real.to_string_lossy()
                ])
                .status()
                .unwrap()
                .success()
        );
        let mut plan = default_execution_plan();
        plan.request.report = Some(real.join("result"));
        plan.request.oci_layout = Some(alias.join("result"));
        assert!(
            validate_outputs(&plan)
                .unwrap_err()
                .to_string()
                .contains("overlap")
        );
        fs::remove_dir(alias).unwrap();
    }

    #[test]
    fn state_local_failure_report_is_skipped_until_state_is_bound() {
        let repository = tempfile::tempdir().unwrap();
        let state = repository.path().join(".repo-sandbox");
        let report = state.join("reports/task.json");
        let destination =
            ReportDestination::prepare(classify_report_destination(&state, &report).unwrap())
                .unwrap();
        assert_eq!(
            optional_failure_report_path(&None, &destination).unwrap(),
            None
        );
        assert!(!state.exists());
    }

    #[test]
    fn relative_state_report_is_classified_by_resolved_identity() {
        let current = std::env::current_dir().unwrap();
        let state = current.join(".repo-sandbox");
        assert!(
            path_is_within_state(&state, Path::new(".repo-sandbox/reports/result.json")).unwrap()
        );
    }

    #[test]
    fn invalid_explicit_report_parent_fails_before_workflow_side_effects() {
        let root = tempfile::tempdir().unwrap();
        let file_parent = root.path().join("Cargo.toml");
        fs::write(&file_parent, "not a directory").unwrap();
        let report = file_parent.join("result.json");
        let error = ExternalReportGuard::prepare(&report).unwrap_err();
        assert!(error.to_string().contains("report destination parent"));
        assert_eq!(fs::read_to_string(file_parent).unwrap(), "not a directory");
    }

    #[test]
    fn invalid_oci_parent_fails_before_workflow_state_or_docker_side_effects() {
        let repository = tempfile::tempdir().unwrap();
        let file_parent = repository.path().join("not-a-directory");
        fs::write(&file_parent, "preserved").unwrap();
        let mut plan = default_execution_plan();
        plan.request.repository = Some(repository.path().to_string_lossy().into_owned());
        plan.request.oci_layout = Some(file_parent.join("layout"));
        let plan = ExecutionPlan::new(plan.template, plan.request);
        let error = WorkflowPort::execute(&SystemWorkflow, WorkflowMode::Build, &plan).unwrap_err();
        assert!(
            error.to_string().contains("OCI layout destination parent"),
            "unexpected error category: {error}"
        );
        assert_eq!(fs::read_to_string(file_parent).unwrap(), "preserved");
        assert!(!repository.path().join(".repo-sandbox").exists());
    }

    #[cfg(unix)]
    #[test]
    fn oci_destination_remains_bound_when_parent_path_is_replaced() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        let original = root.path().join("original");
        let external = root.path().join("external");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&external).unwrap();
        let guard = ExternalOciGuard::prepare(&parent.join("layout")).unwrap();
        fs::rename(&parent, &original).unwrap();
        symlink(&external, &parent).unwrap();

        let bound = guard.bound_path().unwrap();
        fs::create_dir(&bound).unwrap();
        fs::write(bound.join("sentinel"), "owned").unwrap();
        assert_eq!(
            fs::read_to_string(original.join("layout/sentinel")).unwrap(),
            "owned"
        );
        assert!(!external.join("layout").exists());
    }

    #[cfg(windows)]
    #[test]
    fn oci_destination_parent_cannot_be_replaced_while_bound() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        fs::create_dir(&parent).unwrap();
        let guard = ExternalOciGuard::prepare(&parent.join("layout")).unwrap();
        assert!(fs::rename(&parent, root.path().join("replacement")).is_err());
        let bound = guard.bound_path().unwrap();
        assert_eq!(
            bound.parent().unwrap().canonicalize().unwrap(),
            parent.canonicalize().unwrap()
        );
    }

    #[test]
    fn invalid_push_is_rejected_before_an_explicit_report_parent_is_created() {
        let repository = tempfile::tempdir().unwrap();
        let report_parent = repository.path().join("new-report-parent");
        let mut plan = default_execution_plan();
        plan.request.repository = Some(repository.path().to_string_lossy().into_owned());
        plan.request.report = Some(report_parent.join("result.json"));
        plan.request.push = true;
        let plan = ExecutionPlan::new(plan.template, plan.request);
        let error = WorkflowPort::execute(&SystemWorkflow, WorkflowMode::Build, &plan).unwrap_err();
        assert!(error.to_string().contains("--push requires"));
        assert!(!report_parent.exists());
        assert!(!repository.path().join(".repo-sandbox").exists());
    }

    #[test]
    fn external_report_preflight_creates_no_named_probe_and_rolls_back_empty_parents() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("created").join("nested");
        let guard = ExternalReportGuard::prepare(&parent.join("result.json")).unwrap();
        assert!(parent.read_dir().unwrap().next().is_none());
        drop(guard);
        assert!(!root.path().join("created").exists());
    }

    #[test]
    fn external_report_rollback_never_removes_concurrent_content() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("created").join("nested");
        let guard = ExternalReportGuard::prepare(&parent.join("result.json")).unwrap();
        fs::write(parent.join("user-content"), "preserved").unwrap();
        drop(guard);
        assert_eq!(
            fs::read_to_string(parent.join("user-content")).unwrap(),
            "preserved"
        );
    }

    #[test]
    fn distinct_outputs_can_share_one_already_bound_parent() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("outputs");
        fs::create_dir(&parent).unwrap();
        let oci = ExternalOciGuard::prepare(&parent.join("layout")).unwrap();
        let report = ExternalReportGuard::prepare(&parent.join("report.json")).unwrap();
        assert_ne!(oci.bound_path().unwrap(), report.bound_path().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn state_report_remains_bound_after_reports_directory_is_replaced() {
        use std::os::unix::fs::symlink;
        let repository = tempfile::tempdir().unwrap();
        let state = repository.path().join(".repo-sandbox");
        let report = state.join("reports/result.json");
        let destination =
            ReportDestination::prepare(classify_report_destination(&state, &report).unwrap())
                .unwrap();
        let guard = prepare_state_layout(repository.path(), &state).unwrap();
        let original = state.join("reports-original");
        fs::rename(state.join("reports"), &original).unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("sentinel"), "unchanged").unwrap();
        symlink(outside.path(), state.join("reports")).unwrap();
        fs::write(destination.bound_path(Some(&guard)).unwrap(), "report").unwrap();
        assert_eq!(
            fs::read_to_string(original.join("result.json")).unwrap(),
            "report"
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("sentinel")).unwrap(),
            "unchanged"
        );
        assert!(!outside.path().join("result.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn external_report_parent_handle_survives_path_replacement() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("reports");
        fs::create_dir(&parent).unwrap();
        let guard = ExternalReportGuard::prepare(&parent.join("result.json")).unwrap();
        let original = root.path().join("reports-original");
        fs::rename(&parent, &original).unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("sentinel"), "unchanged").unwrap();
        symlink(outside.path(), &parent).unwrap();
        fs::write(guard.bound_path().unwrap(), "report").unwrap();
        assert_eq!(
            fs::read_to_string(original.join("result.json")).unwrap(),
            "report"
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("sentinel")).unwrap(),
            "unchanged"
        );
    }

    #[test]
    fn default_report_does_not_mask_a_pre_state_configuration_failure() {
        let repository = tempfile::tempdir().unwrap();
        let mut plan = default_execution_plan();
        plan.request.repository = Some(repository.path().to_string_lossy().into_owned());
        plan.request.push = true;
        let plan = ExecutionPlan::new(plan.template, plan.request);
        let error = WorkflowPort::execute(&SystemWorkflow, WorkflowMode::Build, &plan).unwrap_err();
        assert!(error.to_string().contains("--push requires"));
        assert!(!repository.path().join(".repo-sandbox").exists());
    }

    #[test]
    fn materialized_configuration_must_match_the_planned_bytes() {
        let root = tempfile::tempdir().unwrap();
        let source = b"version: 1\n";
        fs::write(root.path().join(".repo-sandbox.yaml"), source).unwrap();
        let mut plan = default_execution_plan();
        plan.request.repository_config_digest = Some(configuration_source_digest(source));
        verify_materialized_configuration(&plan, root.path()).unwrap();
        fs::write(root.path().join(".repo-sandbox.yaml"), b"version: 2\n").unwrap();
        assert!(
            verify_materialized_configuration(&plan, root.path())
                .unwrap_err()
                .to_string()
                .contains("differs")
        );
    }

    #[test]
    fn outputs_cannot_replace_required_workflow_state_paths() {
        let repository = tempfile::tempdir().unwrap();
        let state = repository.path().join(".repo-sandbox");
        let default_report = state.join("reports/task.json");
        assert!(validate_state_outputs(&state, &default_report, None).is_ok());
        assert!(
            validate_state_outputs(&state, &default_report, Some(&state))
                .unwrap_err()
                .to_string()
                .contains("oci-layout")
        );
        assert!(
            validate_state_outputs(&state, &state.join("cache"), None)
                .unwrap_err()
                .to_string()
                .contains("report-path")
        );
        assert!(
            validate_state_outputs(&state, repository.path(), None)
                .unwrap_err()
                .to_string()
                .contains("report-path")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_to_future_state_is_rejected_before_state_creation() {
        use std::os::unix::fs::symlink;
        let repository = tempfile::tempdir().unwrap();
        let state = repository.path().join(".repo-sandbox");
        let alias = repository.path().join("alias");
        symlink(repository.path(), &alias).unwrap();
        let report = state.join("reports/task.json");
        let error =
            validate_state_outputs(&state, &report, Some(&alias.join(".repo-sandbox/export")))
                .unwrap_err();
        assert!(error.to_string().contains("oci-layout"));
        let error =
            validate_state_outputs(&state, &alias.join(".repo-sandbox/cache/report.json"), None)
                .unwrap_err();
        assert!(error.to_string().contains("report-path"));
        assert!(!state.exists());
    }

    #[cfg(windows)]
    #[test]
    fn junction_alias_to_future_state_is_rejected_before_state_creation() {
        let repository = tempfile::tempdir().unwrap();
        let state = repository.path().join(".repo-sandbox");
        let alias = repository.path().join("alias");
        assert!(
            std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    &alias.to_string_lossy(),
                    &repository.path().to_string_lossy(),
                ])
                .status()
                .unwrap()
                .success()
        );
        let report = state.join("reports/task.json");
        let error =
            validate_state_outputs(&state, &report, Some(&alias.join(".repo-sandbox/export")))
                .unwrap_err();
        assert!(error.to_string().contains("oci-layout"));
        let error =
            validate_state_outputs(&state, &alias.join(".repo-sandbox/cache/report.json"), None)
                .unwrap_err();
        assert!(error.to_string().contains("report-path"));
        assert!(!state.exists());
        fs::remove_dir(&alias).unwrap();
    }

    #[test]
    fn clean_rejects_a_plan_whose_journal_changed_before_lease_acquisition() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("tasks");
        fs::create_dir(&root).unwrap();
        let plan = CleanPlan {
            journal_revisions: [(root.clone(), Vec::new())].into_iter().collect(),
            ..CleanPlan::default()
        };
        fs::write(root.join("event-00000000000000000001-new.json"), "[]").unwrap();
        let result = execute_clean(&plan, false, &NeverCancelled).unwrap();
        assert!(!result.complete());
        assert!(result.unfinished[0].contains("plan changed"));
    }

    #[test]
    fn workflow_lease_refuses_a_linked_control_file_without_touching_target() {
        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().join("state");
        fs::create_dir(&state).unwrap();
        let sentinel = temporary.path().join("sentinel");
        fs::write(&sentinel, "unchanged").unwrap();
        fs::hard_link(&sentinel, state.join(".workflow.lock")).unwrap();
        let error = WorkflowLease::shared(&state).unwrap_err();
        assert!(error.to_string().contains("single-link"));
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "unchanged");
    }

    #[test]
    fn confirmed_absence_records_a_terminal_cleaned_event() {
        let temporary = tempfile::tempdir().unwrap();
        let missing = temporary.path().join("already-gone.json");
        let candidate = CleanCandidate {
            task_id: "task".into(),
            repository_id: "repository".into(),
            kind: ResourceKind::Cache,
            identifier: missing.display().to_string(),
            owner: "owner".into(),
            state: ResourceState::Registered,
        };
        let plan = CleanPlan {
            candidates: vec![candidate],
            manifest_root: Some(temporary.path().to_path_buf()),
            ..CleanPlan::default()
        };
        let result = execute_clean(&plan, false, &NeverCancelled).unwrap();
        assert!(result.complete());
        assert_eq!(result.absent, vec![missing.display().to_string()]);
        assert!(result.failed.is_empty());
        let event = fs::read_dir(temporary.path())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.path().extension().and_then(|v| v.to_str()) == Some("json"))
            .unwrap();
        let text = fs::read_to_string(event.path()).unwrap();
        assert!(text.contains("\"state\": \"cleaned\""));
    }

    #[test]
    fn task_ids_are_unique_under_same_process_concurrency() {
        let ids = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let threads = (0..32)
            .map(|_| {
                let ids = ids.clone();
                std::thread::spawn(move || ids.lock().unwrap().push(task_id()))
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }
        let ids = ids.lock().unwrap();
        let unique = ids.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), ids.len());
        assert!(ids.iter().all(|id| id.split('-').count() == 3));
    }

    #[test]
    fn task_id_child_process() {
        if std::env::var_os("REPO_SANDBOX_TASK_ID_CHILD").is_some() {
            println!("TASK-ID {}", task_id());
        }
    }

    #[test]
    fn task_ids_include_cross_process_entropy() {
        let executable = std::env::current_exe().unwrap();
        let create = || {
            let output = std::process::Command::new(&executable)
                .args([
                    "--exact",
                    "workflow::tests::task_id_child_process",
                    "--nocapture",
                ])
                .env("REPO_SANDBOX_TASK_ID_CHILD", "1")
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix("TASK-ID "))
                .unwrap()
                .to_owned()
        };
        assert_ne!(create(), create());
    }

    #[test]
    fn concurrent_fresh_state_creation_is_idempotent_and_bound() {
        let repository = tempfile::tempdir().unwrap();
        let root = repository.path().to_path_buf();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let threads = (0..2)
            .map(|_| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    prepare_state_layout(&root, &root.join(".repo-sandbox"))
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let guards = threads
            .into_iter()
            .map(|thread| thread.join().unwrap().unwrap())
            .collect::<Vec<_>>();
        for guard in guards {
            guard.ensure().unwrap();
        }
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
    fn pinned_remote_execution_preserves_the_operator_requested_ref() {
        use repo_sandbox_core::snapshot::CommitSha;
        let mut plan = default_execution_plan();
        plan.request.requested_git_ref = Some("refs/heads/topic".into());
        plan.request.git_ref = Some("a".repeat(40));
        let mut origin = SnapshotOrigin::RemoteGit {
            repository: "https://example.test/repository.git".into(),
            requested_ref: "a".repeat(40),
            commit: CommitSha::parse("a".repeat(40)).unwrap(),
        };
        preserve_requested_ref(&mut origin, &plan.request);
        let SnapshotOrigin::RemoteGit {
            requested_ref,
            commit,
            ..
        } = origin
        else {
            unreachable!()
        };
        assert_eq!(requested_ref, "refs/heads/topic");
        assert_eq!(commit.as_str(), "a".repeat(40));
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
    fn container_platform_contract_failures_have_stable_environment_phase() {
        assert_eq!(
            infrastructure_report_phase("create owned container"),
            "environment"
        );
        assert_eq!(
            infrastructure_report_phase("start owned container"),
            "environment"
        );
        assert_eq!(infrastructure_report_phase("execute job step"), "runner");
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
    fn required_secret_availability_is_validated_before_workflow_side_effects() {
        let mut plan = default_execution_plan();
        let name = format!("REPO_SANDBOX_MISSING_SECRET_{}", task_id());
        plan.template.execution.secret_environment = vec![name.clone()];
        let error = validate_required_secret_environment(&plan).unwrap_err();
        assert!(error.to_string().contains(&name));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_secret_is_rejected_without_rendering_its_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let value = std::ffi::OsString::from_vec(vec![b's', b'e', 0xff, b'c']);
        let error = validated_secret_text("TOKEN", &value).unwrap_err();
        assert!(error.to_string().contains("not valid UTF-8"));
        assert!(!error.to_string().contains('\u{fffd}'));
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
                include_cache: true,
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
            journal_revisions: std::collections::BTreeMap::new(),
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
        let guard = prepare_state_layout(&canonical, &state).unwrap();
        let first = ManifestJournal::create(&state, "original", guard.clone()).unwrap();
        first.append(std::slice::from_ref(&candidate)).unwrap();
        append_cleanup_state(&state.join("tasks"), &candidate).unwrap();
        let mut rebuilt = candidate.clone();
        rebuilt.task_id = "rebuilt".into();
        ManifestJournal::create(&state, "rebuilt", guard)
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

    #[cfg(unix)]
    #[test]
    fn journal_sequence_symlink_never_overwrites_external_file() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), "sentinel").unwrap();
        symlink(outside.path(), root.path().join(".sequence")).unwrap();
        assert!(next_journal_sequence(root.path()).is_err());
        assert_eq!(fs::read_to_string(outside.path()).unwrap(), "sentinel");
    }

    #[cfg(windows)]
    #[test]
    fn journal_sequence_junction_never_writes_external_directory() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, "unchanged").unwrap();
        let sequence = root.path().join(".sequence");
        assert!(
            std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    &sequence.to_string_lossy(),
                    &outside.path().to_string_lossy(),
                ])
                .status()
                .unwrap()
                .success()
        );
        assert!(next_journal_sequence(root.path()).is_err());
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged");
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
        fs::remove_dir(sequence).unwrap();
    }

    #[test]
    fn journal_sequence_hardlink_never_overwrites_external_file() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), "sentinel").unwrap();
        fs::hard_link(outside.path(), root.path().join(".sequence")).unwrap();
        assert!(next_journal_sequence(root.path()).is_err());
        assert_eq!(fs::read_to_string(outside.path()).unwrap(), "sentinel");
    }

    #[test]
    fn cache_owner_marker_hardlink_never_overwrites_external_file() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), "external-sentinel").unwrap();
        let marker = root.path().join(OWNER_MARKER);
        fs::hard_link(outside.path(), &marker).unwrap();
        let error = write_state_file(&marker, b"repository-id").unwrap_err();
        assert!(error.to_string().contains("single-link regular file"));
        assert_eq!(
            fs::read_to_string(outside.path()).unwrap(),
            "external-sentinel"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cache_owner_marker_symlink_never_overwrites_external_file() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), "external-sentinel").unwrap();
        let marker = root.path().join(OWNER_MARKER);
        symlink(outside.path(), &marker).unwrap();
        assert!(write_state_file(&marker, b"repository-id").is_err());
        assert_eq!(
            fs::read_to_string(outside.path()).unwrap(),
            "external-sentinel"
        );
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
    fn workflow_lease_precedes_the_first_journal_event() {
        let repository = tempfile::tempdir().unwrap();
        let state = repository.path().join(".repo-sandbox");
        let (_guard, _lease, _journal) =
            prepare_leased_workflow_state(repository.path(), &state, "task").unwrap();
        assert!(state.join("tasks").read_dir().unwrap().any(|entry| {
            entry
                .ok()
                .is_some_and(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        }));
        assert!(
            WorkflowLease::exclusive(&state.join(".workflow.lock"))
                .unwrap()
                .is_none()
        );
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
