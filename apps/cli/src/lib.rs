use clap::{Args, Parser, Subcommand};
use repo_sandbox_adapters::doctor::{DoctorOptions, DoctorProbe, SystemDoctorProbe, inspect};
use repo_sandbox_adapters::snapshot::GitSnapshotter;
use repo_sandbox_adapters::workflow::SystemWorkflow;
use repo_sandbox_core::AppError;
use repo_sandbox_core::application::{
    BuildUseCase, CleanRequest, CleanUseCase, ExecutionPlan, VerifyUseCase, WorkflowFailureReport,
    WorkflowFailureStatus, configuration_source_digest, write_failure_report,
};
use repo_sandbox_core::config::{
    CliOverrides, Config, ExecutionRequest, Platform, RemoteAuthentication,
};
use repo_sandbox_core::doctor::{CapabilityKind, CapabilityStatus, DoctorReport, DoctorStatus};
use repo_sandbox_core::exit_code::ExitCode;
use repo_sandbox_core::snapshot::{CleanupPolicy, SnapshotOptions, SnapshotOrigin, SourceSpec};
use repo_sandbox_core::template::{TemplateCatalog, TemplatePlan};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as IoWrite};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const REMOTE_PREPARATION_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Parser)]
#[command(
    name = "repo-sandbox",
    version,
    about = "Build and verify repository work in isolated sandboxes"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

impl Cli {
    /// Only commands whose adapters cooperatively observe cancellation install
    /// the process-wide handler. Doctor intentionally retains the OS default
    /// Ctrl-C behavior for its blocking prerequisite probes.
    pub fn requires_interrupt_handler(&self) -> bool {
        matches!(
            &self.command,
            Some(Commands::Plan(_) | Commands::Build(_) | Commands::Verify(_))
        ) || matches!(&self.command, Some(Commands::Clean(args)) if args.yes || args.dry_run
        )
    }
}

#[derive(Clone, Debug, Subcommand)]
enum Commands {
    /// Inspect local prerequisites without modifying the host.
    Doctor(DoctorArgs),
    /// Resolve the selected central template and display its dependency graph.
    Plan(RuntimeArgs),
    /// Build in a bounded one-shot sandbox and export declared artifacts.
    Build(RuntimeArgs),
    /// Build and test in a bounded one-shot sandbox.
    Verify(RuntimeArgs),
    /// Remove only resources proven to be owned by repo-sandbox.
    Clean(CleanArgs),
}

#[derive(Args, Clone, Debug, Default, Eq, PartialEq)]
pub struct DoctorArgs {
    /// Emit the same capability conclusions as structured JSON.
    #[arg(long)]
    pub json: bool,
}

/// The complete and intentionally finite v1 CLI override surface.
#[derive(Args, Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeArgs {
    /// Repository path or URL to operate on.
    #[arg(long, value_name = "PATH_OR_URL")]
    pub repository: Option<String>,
    /// Git ref to check out in the sandbox.
    #[arg(long = "git-ref", value_name = "REF")]
    pub git_ref: Option<String>,
    /// Resolved immutable ref used internally after remote configuration discovery.
    #[arg(skip)]
    resolved_git_ref: Option<String>,
    /// Override the repository-declared target platform.
    #[arg(long, value_name = "PLATFORM")]
    pub platform: Vec<Platform>,
    /// Atomically export a verified OCI image layout.
    #[arg(long = "oci-layout", value_name = "DIRECTORY")]
    pub oci_layout: Option<PathBuf>,
    /// Push the verified task image using the central registry policy.
    #[arg(long)]
    pub push: bool,
    /// Atomically write the machine-readable report; never overwrites.
    #[arg(long = "report-path", value_name = "PATH")]
    pub report: Option<PathBuf>,
    /// Preserve a failed sandbox for diagnosis.
    #[arg(long)]
    pub keep_on_failure: bool,
    /// Recursively materialize Git submodules in the source snapshot.
    #[arg(long)]
    pub recurse_submodules: bool,
    /// Name of an environment variable containing an HTTPS token.
    #[arg(long = "git-https-token-env", value_name = "NAME")]
    pub git_https_token_env: Option<String>,
    /// HTTPS username paired with --git-https-token-env.
    #[arg(long = "git-https-username", value_name = "USER")]
    pub git_https_username: Option<String>,
    /// Use the operator-configured Git HTTPS credential helper.
    #[arg(long = "git-credential-helper")]
    pub git_credential_helper: bool,
    /// Path to an external SSH private key; key bytes never enter the plan.
    #[arg(long = "git-ssh-private-key", value_name = "PATH")]
    pub git_ssh_private_key: Option<PathBuf>,
    /// Path to the strict SSH known_hosts file.
    #[arg(long = "git-ssh-known-hosts", value_name = "PATH")]
    pub git_ssh_known_hosts: Option<PathBuf>,
    /// Use the external SSH agent instead of a private-key file.
    #[arg(long = "git-ssh-agent")]
    pub git_ssh_agent: bool,
}

#[derive(Args, Clone, Debug, Default, Eq, PartialEq)]
pub struct CleanArgs {
    /// Repository whose task manifest establishes the ownership boundary.
    #[arg(long, value_name = "PATH", default_value = ".")]
    pub repository: PathBuf,
    /// Show the exact cleanup plan without modifying Docker or files.
    #[arg(long)]
    pub dry_run: bool,
    /// Include every repo-sandbox-owned entry in this manifest store.
    #[arg(long)]
    pub all: bool,
    /// Also remove task images after label and reference checks.
    #[arg(long)]
    pub include_images: bool,
    /// Also remove the repository-owned local cache directory.
    #[arg(long)]
    pub include_cache: bool,
    /// Confirm non-interactively (required unless --dry-run).
    #[arg(long)]
    pub yes: bool,
}

impl From<RuntimeArgs> for CliOverrides {
    fn from(value: RuntimeArgs) -> Self {
        let requested_git_ref = value.git_ref.clone();
        Self {
            repository: value.repository,
            requested_git_ref,
            git_ref: value.resolved_git_ref.or(value.git_ref),
            platform: value.platform.first().copied(),
            platforms: value.platform,
            oci_layout: value.oci_layout,
            push: value.push,
            report: value.report,
            keep_on_failure: value.keep_on_failure,
            recurse_submodules: value.recurse_submodules,
            remote_auth: RemoteAuthentication {
                https_username: value.git_https_username,
                https_token_environment: value.git_https_token_env,
                https_credential_helper: value.git_credential_helper,
                ssh_private_key: value.git_ssh_private_key,
                ssh_known_hosts: value.git_ssh_known_hosts,
                ssh_agent: value.git_ssh_agent,
            },
            repository_config_digest: None,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct RunOutput {
    pub message: Option<String>,
    pub exit_code: ExitCode,
}

pub fn run(cli: Cli) -> Result<RunOutput, AppError> {
    run_with_probe(cli, &SystemDoctorProbe)
}

pub fn run_with_probe(cli: Cli, probe: &impl DoctorProbe) -> Result<RunOutput, AppError> {
    let Some(command) = cli.command else {
        return Ok(RunOutput {
            message: None,
            exit_code: ExitCode::Success,
        });
    };
    if let Commands::Doctor(arguments) = command {
        let report = inspect(probe, &DoctorOptions::default());
        let message = if arguments.json {
            render_json(&report)
        } else {
            render_human(&report)
        };
        return Ok(RunOutput {
            message: Some(message),
            exit_code: if report.is_ready() {
                ExitCode::Success
            } else {
                ExitCode::Environment
            },
        });
    }
    if let Commands::Plan(arguments) = command {
        let cancellation = repo_sandbox_adapters::cancellation::DeadlineCancellation::new(
            REMOTE_PREPARATION_TIMEOUT,
        );
        let execution = prepare_execution_cancellable(arguments, &cancellation)?;
        repo_sandbox_adapters::workflow::validate_outputs(&execution)?;
        execution.validate_mode(repo_sandbox_core::application::WorkflowMode::Verify)?;
        return Ok(RunOutput {
            message: Some(render_plan(&execution.template)),
            exit_code: ExitCode::Success,
        });
    }
    let workflow = SystemWorkflow;
    match command {
        Commands::Build(arguments) => run_runtime(arguments, false, &workflow),
        Commands::Verify(arguments) => run_runtime(arguments, true, &workflow),
        Commands::Clean(arguments) => {
            let request = CleanRequest {
                repository: arguments.repository,
                all: arguments.all,
                include_images: arguments.include_images,
                include_cache: arguments.include_cache,
                dry_run: arguments.dry_run,
            };
            let use_case = CleanUseCase::new(&workflow);
            let plan = use_case.plan(&request)?;
            if !request.dry_run && !arguments.yes {
                eprintln!("{}", render_clean_plan(&plan));
                eprint!("Remove these owned resources? [y/N] ");
                io::stderr()
                    .flush()
                    .map_err(|error| AppError::Environment(error.to_string()))?;
                let mut answer = String::new();
                io::stdin()
                    .read_line(&mut answer)
                    .map_err(|error| AppError::Environment(error.to_string()))?;
                if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
                    return Ok(RunOutput {
                        message: Some("clean cancelled; no resources changed".into()),
                        exit_code: ExitCode::Success,
                    });
                }
            }
            if !request.dry_run && !arguments.yes {
                repo_sandbox_adapters::cancellation::install().map_err(|error| {
                    AppError::Environment(format!("cannot install interrupt handler: {error}"))
                })?;
            }
            let result = use_case.execute(&plan, request.dry_run)?;
            let message = render_clean_result(&plan, &result);
            Ok(RunOutput {
                message: Some(message),
                exit_code: if result.complete() {
                    ExitCode::Success
                } else {
                    ExitCode::Environment
                },
            })
        }
        Commands::Doctor(_) | Commands::Plan(_) => unreachable!(),
    }
}

fn render_clean_plan(plan: &repo_sandbox_core::application::CleanPlan) -> String {
    let mut lines = vec![format!(
        "clean plan: {} candidate(s), {} refused",
        plan.candidates.len(),
        plan.refused.len()
    )];
    for item in &plan.candidates {
        lines.push(format!(
            "  candidate {:?} {} task={}",
            item.kind, item.identifier, item.task_id
        ));
    }
    for reason in &plan.refused {
        lines.push(format!("  refused {reason}"));
    }
    lines.join("\n")
}

fn render_clean_result(
    plan: &repo_sandbox_core::application::CleanPlan,
    result: &repo_sandbox_core::application::CleanResult,
) -> String {
    let mut lines = vec![
        render_clean_plan(plan),
        format!(
            "clean: {} succeeded, {} skipped, {} absent, {} unfinished, {} failed{}",
            result.succeeded.len(),
            result.skipped.len(),
            result.absent.len(),
            result.unfinished.len(),
            result.failed.len(),
            if result.dry_run { " (dry-run)" } else { "" }
        ),
    ];
    for item in &result.succeeded {
        lines.push(format!("  removed {:?} {}", item.kind, item.identifier));
    }
    for item in &result.skipped {
        lines.push(format!("  skipped {item}"));
    }
    for item in &result.absent {
        lines.push(format!("  absent {item}"));
    }
    for item in &result.unfinished {
        lines.push(format!("  unfinished {item}"));
    }
    for item in &result.failed {
        lines.push(format!("  failed {item}"));
    }
    lines.join("\n")
}

fn run_runtime(
    arguments: RuntimeArgs,
    verify: bool,
    workflow: &SystemWorkflow,
) -> Result<RunOutput, AppError> {
    let started = Instant::now();
    let preparation_cancellation = repo_sandbox_adapters::cancellation::DeadlineCancellation::at(
        started + REMOTE_PREPARATION_TIMEOUT,
    );
    let requested_report = arguments.report.clone();
    let requested_repository = arguments.repository.clone();
    let report_reservation = requested_report
        .as_deref()
        .map(repo_sandbox_adapters::workflow::OutputReservation::report)
        .transpose()?;
    let prepared = prepare_execution_cancellable(arguments, &preparation_cancellation);
    let mut execution = match prepared {
        Ok(execution) => execution,
        Err(error) => {
            if let Some(path) = requested_report.as_deref()
                && !path.exists()
            {
                write_cli_failure_report(
                    path,
                    requested_repository.as_deref(),
                    "unavailable",
                    preparation_phase(&error),
                    &error,
                )?;
            }
            return Err(error);
        }
    };
    execution.deadline = Some(
        started + Duration::from_secs(u64::from(execution.template.execution.timeout_seconds)),
    );
    // Preparation failures are protected by the CLI reservation. The workflow
    // immediately acquires the same cross-process reservation for execution.
    drop(report_reservation);
    let result = if verify {
        VerifyUseCase::new(workflow).execute(&execution)
    } else {
        BuildUseCase::new(workflow).execute(&execution)
    };
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            if let Some(path) = &execution.request.report
                && !path.exists()
            {
                write_cli_failure_report(
                    path,
                    execution.request.repository.as_deref(),
                    &execution.digest,
                    "orchestration",
                    &error,
                )?;
            }
            return Err(error);
        }
    };
    Ok(RunOutput {
        message: Some(render_workflow(&result)),
        exit_code: ExitCode::Success,
    })
}

#[cfg(test)]
fn prepare_execution(arguments: RuntimeArgs) -> Result<ExecutionPlan, AppError> {
    prepare_execution_cancellable(arguments, &repo_sandbox_adapters::buildkit::NeverCancelled)
}

fn prepare_execution_cancellable(
    mut arguments: RuntimeArgs,
    cancellation: &dyn repo_sandbox_adapters::buildkit::Cancellation,
) -> Result<ExecutionPlan, AppError> {
    arguments.git_ssh_private_key =
        canonicalize_credential_path(arguments.git_ssh_private_key.take(), "SSH private key")?;
    arguments.git_ssh_known_hosts =
        canonicalize_credential_path(arguments.git_ssh_known_hosts.take(), "SSH known_hosts")?;
    if arguments.recurse_submodules && arguments.git_https_token_env.is_some() {
        return Err(AppError::Configuration(
            "--recurse-submodules with --git-https-token-env requires separately scoped submodule credentials, which are not supported in v1".into(),
        ));
    }
    if let Some(repository) = arguments
        .repository
        .as_deref()
        .filter(|value| repo_sandbox_adapters::workflow::is_remote_repository(value))
    {
        repo_sandbox_adapters::workflow::validate_remote_repository(repository)?;
    }
    let remote_auth = RemoteAuthentication {
        https_username: arguments.git_https_username.clone(),
        https_token_environment: arguments.git_https_token_env.clone(),
        https_credential_helper: arguments.git_credential_helper,
        ssh_private_key: arguments.git_ssh_private_key.clone(),
        ssh_known_hosts: arguments.git_ssh_known_hosts.clone(),
        ssh_agent: arguments.git_ssh_agent,
    };
    let source = if let Some(repository) = arguments
        .repository
        .as_deref()
        .filter(|value| repo_sandbox_adapters::workflow::is_remote_repository(value))
    {
        let materialized = GitSnapshotter::default()
            .with_authentication(
                repo_sandbox_adapters::workflow::external_git_authentication(
                    repository,
                    &remote_auth,
                )?,
            )
            .create_cancellable(
                &SourceSpec::RemoteGit {
                    repository: repository.to_owned(),
                    git_ref: arguments.git_ref.clone().unwrap_or_else(|| "HEAD".into()),
                },
                SnapshotOptions {
                    // Configuration discovery only needs the root tree. The execution
                    // snapshot materializes submodules once, using the pinned commit.
                    recurse_submodules: false,
                    cleanup: CleanupPolicy::Delete,
                },
                cancellation,
            )
            .map_err(|error| AppError::Environment(error.to_string()))?;
        if let SnapshotOrigin::RemoteGit { commit, .. } = &materialized.snapshot.origin {
            arguments.resolved_git_ref = Some(commit.as_str().to_owned());
        }
        fs::read_to_string(materialized.path().join(".repo-sandbox.yaml")).map_err(|error| {
            AppError::Configuration(format!("remote .repo-sandbox.yaml: {error}"))
        })?
    } else {
        if arguments.git_ref.is_some() {
            return Err(AppError::Configuration(
                "--git-ref is supported only with a remote repository URL".into(),
            ));
        }
        repo_sandbox_adapters::workflow::external_git_authentication("", &remote_auth)?;
        read_repository_config(arguments.repository.as_deref())?
    };
    execution_plan_from_source(&source, arguments)
}

fn canonicalize_credential_path(
    path: Option<PathBuf>,
    description: &str,
) -> Result<Option<PathBuf>, AppError> {
    canonicalize_credential_path_from(
        path,
        description,
        &std::env::current_dir().map_err(|error| {
            AppError::Configuration(format!("cannot resolve invocation directory: {error}"))
        })?,
    )
}

fn canonicalize_credential_path_from(
    path: Option<PathBuf>,
    description: &str,
    invocation_directory: &std::path::Path,
) -> Result<Option<PathBuf>, AppError> {
    path.map(|path| {
        let candidate = if path.is_absolute() {
            path.clone()
        } else {
            invocation_directory.join(&path)
        };
        candidate.canonicalize().map_err(|error| {
            AppError::Configuration(format!(
                "cannot resolve {description} {}: {error}",
                path.display()
            ))
        })
    })
    .transpose()
}

fn preparation_phase(error: &AppError) -> &'static str {
    match error {
        AppError::Configuration(_) => "configuration",
        _ => "snapshot",
    }
}

// Finish bounded failure bookkeeping after a workflow cancellation, without
// reusing the consumed process signal or allowing Git validation to hang.
struct FailureReportDeadline(Instant);

impl repo_sandbox_adapters::buildkit::Cancellation for FailureReportDeadline {
    fn is_cancelled(&self) -> bool {
        Instant::now() >= self.0
    }
}

fn write_cli_failure_report(
    path: &std::path::Path,
    repository: Option<&str>,
    plan_digest: &str,
    phase: &'static str,
    error: &AppError,
) -> Result<(), AppError> {
    // Failure fallback must obey the same source/output boundary as a normal
    // workflow, including when configuration discovery never produced a plan.
    if repo_sandbox_adapters::workflow::validate_repository_output_boundary(
        repository,
        path,
        &FailureReportDeadline(Instant::now() + Duration::from_secs(5)),
    )
    .is_err()
    {
        return Ok(());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    fs::create_dir_all(parent).map_err(|write| {
        AppError::Environment(format!(
            "create failure report parent: {write}; primary: {error}"
        ))
    })?;
    let report = WorkflowFailureReport {
        schema_version: 1,
        task_id: "unassigned".into(),
        plan_digest: plan_digest.into(),
        phase: phase.into(),
        exit_code: error.exit_code().as_i32(),
        message: error.to_string(),
        cleanup: repo_sandbox_core::runner::CleanupResult::NotNeeded,
        published: None,
        publication_progress: Vec::new(),
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
            operation: phase.into(),
            message: error.to_string(),
        },
        steps: Vec::new(),
        exported_artifacts: Vec::new(),
        artifact_error: None,
        cleanup_error: None,
    };
    write_failure_report(&report, path).map_err(|write| {
        AppError::Environment(format!("write failure report: {write}; primary: {error}"))
    })
}

fn execution_plan_from_source(
    source: &str,
    arguments: RuntimeArgs,
) -> Result<ExecutionPlan, AppError> {
    let config =
        Config::parse_yaml(source).map_err(|error| AppError::Configuration(error.to_string()))?;
    if config.legacy.is_some() {
        return Err(AppError::Configuration(
            "legacy inline execution is unsupported; select a central template profile".into(),
        ));
    }
    if config.template.id.starts_with("rust-bazel-acceptance-")
        && std::env::var("REPO_SANDBOX_ENABLE_ACCEPTANCE_PROFILES").as_deref() != Ok("1")
    {
        return Err(AppError::Configuration(format!(
            "central diagnostic profile `{}` requires REPO_SANDBOX_ENABLE_ACCEPTANCE_PROFILES=1",
            config.template.id
        )));
    }
    let mut overrides: CliOverrides = arguments.into();
    overrides.repository_config_digest = Some(configuration_source_digest(source.as_bytes()));
    let request = ExecutionRequest::resolve(&config, overrides);
    let template = TemplateCatalog::builtin()
        .map_err(|error| AppError::Configuration(error.to_string()))?
        .plan(&config.template, request.platform)
        .map_err(|error| AppError::Configuration(error.to_string()))?;
    Ok(ExecutionPlan::new(template, request))
}

fn render_workflow(result: &repo_sandbox_core::application::WorkflowResult) -> String {
    format!(
        "task={} status={:?} source=sha256:{} image={}@{} plan={}",
        result.report.task_id,
        result.report.status,
        result.report.source_snapshot.id,
        result.report.image,
        result.report.image_digest,
        result.plan_digest
    )
}

fn read_repository_config(repository: Option<&str>) -> Result<String, AppError> {
    let root = match repository {
        Some(value) if value.contains("://") || value.starts_with("git@") => {
            return Err(AppError::Configuration(
                "plan requires a materialized local repository; remote snapshot planning is not implemented"
                    .to_owned(),
            ));
        }
        Some(value) => PathBuf::from(value),
        None => std::env::current_dir().map_err(|error| {
            AppError::Configuration(format!("cannot determine current repository: {error}"))
        })?,
    };
    let path = root.join(".repo-sandbox.yaml");
    fs::read_to_string(&path)
        .map_err(|error| AppError::Configuration(format!("{}: {error}", path.display())))
}

pub fn plan_from_source(source: &str, arguments: RuntimeArgs) -> Result<RunOutput, AppError> {
    let config =
        Config::parse_yaml(source).map_err(|error| AppError::Configuration(error.to_string()))?;
    if config.legacy.is_some() {
        return Err(AppError::Configuration(
            "$.template: legacy inline template configuration cannot be planned; migrate to `template.id` and `template.parameters`"
                .to_owned(),
        ));
    }
    let request = ExecutionRequest::resolve(&config, arguments.into());
    let catalog = TemplateCatalog::builtin()
        .map_err(|error| AppError::Configuration(format!("central catalog {error}")))?;
    let plan = catalog
        .plan(&config.template, request.platform)
        .map_err(|error| AppError::Configuration(error.to_string()))?;
    let execution = ExecutionPlan::new(plan, request);
    repo_sandbox_adapters::workflow::validate_outputs(&execution)?;
    execution.validate_mode(repo_sandbox_core::application::WorkflowMode::Verify)?;
    Ok(RunOutput {
        message: Some(render_plan(&execution.template)),
        exit_code: ExitCode::Success,
    })
}

pub fn render_plan(plan: &TemplatePlan) -> String {
    let mut lines = vec![
        format!("Template: {}@{}", plan.template_id, plan.template_version),
        format!("Platform: {}", plan.platform),
        format!("Base image: {}", plan.base_image),
        format!("Build context: {}", plan.build_context.display()),
        "Resolved dependency graph:".to_owned(),
    ];
    for (index, stage) in plan.stages.iter().enumerate() {
        let dependencies = if stage.depends_on.is_empty() {
            "(root)".to_owned()
        } else {
            stage.depends_on.join(", ")
        };
        lines.push(format!(
            "  [{index}] {}@{} <- {} [{}]",
            stage.id,
            stage.version,
            dependencies,
            stage.build_context.display()
        ));
    }
    lines.push("Execution profile:".to_owned());
    for step in &plan.execution.build {
        lines.push(format!("  build {}: {}", step.name, step.command));
    }
    for step in &plan.execution.test {
        lines.push(format!("  test {}: {}", step.name, step.command));
    }
    lines.push(format!(
        "  resources: cpu={} memory={}MiB temporary-storage={}MiB timeout={}s fail-fast={}",
        plan.execution.resources.cpu,
        plan.execution.resources.memory_mb,
        plan.execution.resources.temporary_storage_mb,
        plan.execution.timeout_seconds,
        plan.execution.fail_fast
    ));
    lines.join("\n")
}

pub fn render_json(report: &DoctorReport) -> String {
    let status = match report.status {
        DoctorStatus::Ready => "ready",
        DoctorStatus::NotReady => "not_ready",
    };
    let mut output = format!("{{\n  \"status\": \"{status}\",\n  \"capabilities\": [");
    for (index, capability) in report.capabilities.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str("\n    {\n      \"kind\": ");
        push_json_string(&mut output, capability_name(capability.kind));
        output.push_str(",\n      \"status\": ");
        push_json_string(
            &mut output,
            match capability.status {
                CapabilityStatus::Available => "available",
                CapabilityStatus::Unavailable => "unavailable",
            },
        );
        output.push_str(",\n      \"summary\": ");
        push_json_string(&mut output, &capability.summary);
        output.push_str(",\n      \"remediation\": [");
        for (action_index, action) in capability.remediation.iter().enumerate() {
            if action_index != 0 {
                output.push_str(", ");
            }
            push_json_string(&mut output, action);
        }
        output.push_str("]\n    }");
    }
    output.push_str("\n  ]\n}");
    output
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            control if control <= '\u{1f}' => {
                write!(output, "\\u{:04x}", control as u32)
                    .expect("formatting into a String cannot fail");
            }
            printable => output.push(printable),
        }
    }
    output.push('"');
}

pub fn render_human(report: &DoctorReport) -> String {
    let mut lines = vec![format!(
        "repo-sandbox doctor: {}",
        match report.status {
            DoctorStatus::Ready => "ready",
            DoctorStatus::NotReady => "not ready",
        }
    )];
    for capability in &report.capabilities {
        lines.push(format!(
            "[{}] {}: {}",
            match capability.status {
                CapabilityStatus::Available => "available",
                CapabilityStatus::Unavailable => "unavailable",
            },
            capability_name(capability.kind),
            capability.summary
        ));
        for action in &capability.remediation {
            lines.push(format!("  Fix: {action}"));
        }
    }
    lines.join("\n")
}

const fn capability_name(kind: CapabilityKind) -> &'static str {
    match kind {
        CapabilityKind::OperatingSystem => "operating_system",
        CapabilityKind::CpuArchitecture => "cpu_architecture",
        CapabilityKind::DockerDaemon => "docker_daemon",
        CapabilityKind::Buildkit => "buildkit",
        CapabilityKind::Buildx => "buildx",
        CapabilityKind::QemuBinfmt => "qemu_binfmt",
        CapabilityKind::DiskSpace => "disk_space",
        CapabilityKind::RegistryConnectivity => "registry_connectivity",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use repo_sandbox_adapters::doctor::{CommandInvocation, CommandOutput};
    use std::io;
    use std::path::Path;
    use std::time::Duration;

    struct ReadyProbe;

    impl DoctorProbe for ReadyProbe {
        fn os(&self) -> String {
            "linux".to_owned()
        }

        fn architecture(&self) -> String {
            "x86_64".to_owned()
        }

        fn execute(&self, invocation: &CommandInvocation) -> io::Result<CommandOutput> {
            let stdout = if invocation.args == ["buildx", "inspect"] {
                "Status: running\nPlatforms: linux/amd64, linux/arm64"
            } else {
                "available"
            };
            Ok(CommandOutput {
                success: true,
                stdout: stdout.to_owned(),
                stderr: String::new(),
            })
        }

        fn available_space(&self, _path: &Path) -> io::Result<u64> {
            Ok(20 * 1024 * 1024 * 1024)
        }

        fn connect_registry(&self, _host: &str, _port: u16, _timeout: Duration) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn relative_ssh_credential_paths_are_resolved_from_invocation_directory() {
        let invocation = std::env::temp_dir().join(format!(
            "repo-sandbox-cli-credential-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir(&invocation).unwrap();
        fs::write(invocation.join("key"), "fixture").unwrap();
        let resolved = canonicalize_credential_path_from(
            Some(PathBuf::from("key")),
            "SSH private key",
            &invocation,
        )
        .unwrap()
        .unwrap();
        assert!(resolved.is_absolute());
        assert_eq!(resolved, invocation.join("key").canonicalize().unwrap());
        fs::remove_file(invocation.join("key")).unwrap();
        fs::remove_dir(invocation).unwrap();
    }

    #[test]
    fn only_cancellation_aware_commands_install_the_interrupt_handler() {
        {
            let command = "doctor";
            let cli = Cli::try_parse_from(["repo-sandbox", command]).unwrap();
            assert!(!cli.requires_interrupt_handler(), "{command}");
            assert!(
                !Cli::try_parse_from(["repo-sandbox", "clean"])
                    .unwrap()
                    .requires_interrupt_handler()
            );
            assert!(
                Cli::try_parse_from(["repo-sandbox", "clean", "--yes"])
                    .unwrap()
                    .requires_interrupt_handler()
            );
        }
        for command in ["plan", "build", "verify"] {
            let cli = Cli::try_parse_from(["repo-sandbox", command]).unwrap();
            assert!(cli.requires_interrupt_handler(), "{command}");
        }
    }

    #[test]
    fn help_is_a_successful_smoke_path() {
        let error = Cli::try_parse_from(["repo-sandbox", "--help"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(error.to_string().contains("doctor"));
    }

    #[test]
    fn version_is_a_successful_smoke_path() {
        let error = Cli::try_parse_from(["repo-sandbox", "--version"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(error.to_string().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn implemented_subcommands_parse_with_their_finite_surfaces() {
        let doctor = Cli::try_parse_from(["repo-sandbox", "doctor"]).unwrap();
        let result = run_with_probe(doctor, &ReadyProbe).unwrap();
        assert_eq!(result.exit_code, ExitCode::Success);
        assert!(result.message.unwrap().contains("doctor: ready"));

        for name in ["build", "verify"] {
            let cli = Cli::try_parse_from(["repo-sandbox", name]).unwrap();
            assert!(matches!(
                cli.command,
                Some(Commands::Build(_)) | Some(Commands::Verify(_))
            ));
        }
        let clean = Cli::try_parse_from(["repo-sandbox", "clean", "--dry-run", "--include-images"])
            .unwrap();
        assert!(matches!(
            clean.command,
            Some(Commands::Clean(CleanArgs {
                dry_run: true,
                include_images: true,
                ..
            }))
        ));
    }

    #[test]
    fn plan_displays_the_resolved_dependency_graph() {
        let source = r#"
version: 1
template:
  id: rust-bazel
  parameters:
    platform: linux/amd64
    rust_version: "1.97.0"
"#;
        let output = plan_from_source(source, RuntimeArgs::default()).unwrap();
        let message = output.message.unwrap();
        assert!(message.contains("Template: rust-bazel@1.0.1"));
        assert!(message.contains("Resolved dependency graph:"));
        assert!(message.contains("[0] base-tools@1.0.0 <- (root)"));
        assert!(message.contains("[1] bazel@1.0.0 <- base-tools"));
        assert!(message.contains("[2] rust@1.0.0 <- base-tools"));
        assert!(message.contains("build bazel-build: bazel build //..."));
    }

    #[test]
    fn legacy_inline_config_gets_an_explicit_migration_error() {
        let error = plan_from_source(
            r#"
version: 1
template:
  name: rust
  platform: linux/amd64
  image: rust:1.97
  timeout_seconds: 1
  resources: { cpu: 1, memory_mb: 512 }
  environment: { allow: [], secrets: [] }
  artifacts: { directories: [target] }
build: [{ name: build, run: cargo build }]
test: [{ name: test, run: cargo test }]
"#,
            RuntimeArgs::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("migrate"));
        assert_eq!(error.exit_code(), ExitCode::Configuration);
    }

    #[test]
    fn configuration_failure_writes_the_requested_common_report_schema() {
        let temporary = std::env::temp_dir().join(format!(
            "repo-sandbox-cli-report-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&temporary).unwrap();
        let repository = temporary.join("repository");
        fs::create_dir(&repository).unwrap();
        fs::write(
            repository.join(".repo-sandbox.yaml"),
            "version: 1\ntemplate: definitely-not-valid\n",
        )
        .unwrap();
        let report = temporary.join("failure.json");
        let cli = Cli::try_parse_from([
            "repo-sandbox",
            "build",
            "--repository",
            repository.to_str().unwrap(),
            "--report-path",
            report.to_str().unwrap(),
        ])
        .unwrap();
        let error = run(cli).unwrap_err();
        assert_eq!(error.exit_code(), ExitCode::Configuration);
        let json = fs::read_to_string(&report).unwrap();
        for field in [
            "schema_version",
            "task_id",
            "plan_digest",
            "phase",
            "exit_code",
            "message",
            "cleanup",
            "published",
            "publication_progress",
            "container_id",
            "source_snapshot",
            "config",
            "image",
            "image_digest",
            "status",
            "steps",
            "exported_artifacts",
        ] {
            assert!(
                json.contains(&format!("\"{field}\"")),
                "missing field {field}"
            );
        }
        assert!(json.contains("\"phase\": \"configuration\""));
        assert!(json.contains("\"exit_code\": 2"));
        fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn failure_reports_respect_the_source_output_boundary() {
        let temporary = std::env::temp_dir().join(format!(
            "cli-report-boundary-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&temporary).unwrap();
        let repository = temporary.join("repository");
        fs::create_dir(&repository).unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(&repository)
                .status()
                .unwrap()
                .success()
        );
        fs::write(repository.join(".gitignore"), "ignored-reports/\n").unwrap();
        let configuration = repository.join(".repo-sandbox.yaml");
        // Cover both preparation failures and the workflow rejecting the same
        // output after successful configuration preparation.
        for source in [
            "version: 1\ntemplate: invalid\n",
            "version: 1\ntemplate:\n  id: rust-bazel\n  parameters:\n    platform: linux/amd64\n",
        ] {
            fs::write(&configuration, source).unwrap();
            let report = repository.join("unignored-reports/failure.json");
            let cli = Cli::try_parse_from([
                "repo-sandbox",
                "build",
                "--repository",
                repository.to_str().unwrap(),
                "--report-path",
                report.to_str().unwrap(),
            ])
            .unwrap();
            let error = run(cli).unwrap_err();
            assert_eq!(error.exit_code(), ExitCode::Configuration);
            if source.contains("rust-bazel") {
                assert!(error.to_string().contains("not Git-ignored"));
            }
            assert!(!report.parent().unwrap().exists());
            assert!(!repository.join(".repo-sandbox").exists());
        }
        for source in [
            "version: 1\ntemplate: invalid\n",
            "version: 1\ntemplate:\n  id: rust-bazel\n  parameters:\n    platform: linux/amd64\n",
        ] {
            fs::write(&configuration, source).unwrap();
            for leaf in [
                "",
                "cache",
                "cache/failure.json",
                "tasks",
                "tasks/failure.json",
                "reports",
            ] {
                let report = repository.join(".repo-sandbox").join(leaf);
                let cli = Cli::try_parse_from([
                    "repo-sandbox",
                    "build",
                    "--repository",
                    repository.to_str().unwrap(),
                    "--report-path",
                    report.to_str().unwrap(),
                ])
                .unwrap();
                assert_eq!(run(cli).unwrap_err().exit_code(), ExitCode::Configuration);
                assert!(!repository.join(".repo-sandbox").exists());
            }
        }
        fs::write(&configuration, "version: 1\ntemplate: invalid\n").unwrap();
        for report in [
            repository.join(".repo-sandbox/reports/failure.json"),
            repository.join("ignored-reports/failure.json"),
            temporary.join("external-reports/failure.json"),
        ] {
            let cli = Cli::try_parse_from([
                "repo-sandbox",
                "build",
                "--repository",
                repository.to_str().unwrap(),
                "--report-path",
                report.to_str().unwrap(),
            ])
            .unwrap();
            assert_eq!(run(cli).unwrap_err().exit_code(), ExitCode::Configuration);
            assert!(
                fs::read_to_string(report)
                    .unwrap()
                    .contains("configuration")
            );
        }
        fs::remove_dir_all(temporary).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn failure_report_cannot_follow_a_state_symlink_but_can_write_externally() {
        let temporary = std::env::temp_dir().join(format!(
            "cli-state-report-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repository = temporary.join("repository");
        let outside = temporary.join("outside");
        fs::create_dir_all(&repository).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(
            repository.join(".repo-sandbox.yaml"),
            "version: 1\ntemplate: invalid\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, repository.join(".repo-sandbox")).unwrap();
        for report in [
            repository.join(".repo-sandbox/reports/failure.json"),
            temporary.join("external.json"),
        ] {
            let cli = Cli::try_parse_from([
                "repo-sandbox",
                "build",
                "--repository",
                repository.to_str().unwrap(),
                "--report-path",
                report.to_str().unwrap(),
            ])
            .unwrap();
            assert_eq!(run(cli).unwrap_err().exit_code(), ExitCode::Configuration);
        }
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        assert!(temporary.join("external.json").is_file());
        fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn configuration_failure_writes_a_bare_relative_report() {
        let name = format!(
            "cli-failure-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let cli = Cli::try_parse_from([
            "repo-sandbox",
            "build",
            "--repository",
            "/nonexistent-repo-sandbox-cli-fixture",
            "--report-path",
            &name,
        ])
        .unwrap();
        assert_eq!(run(cli).unwrap_err().exit_code(), ExitCode::Configuration);
        assert!(fs::read_to_string(&name).unwrap().contains("configuration"));
        fs::remove_file(name).unwrap();
    }

    #[test]
    fn plans_reject_invalid_output_overrides() {
        let source = "version: 1\ntemplate:\n  id: rust-bazel\n";
        for arguments in [
            RuntimeArgs {
                platform: vec![Platform::LinuxAmd64, Platform::LinuxAmd64],
                ..Default::default()
            },
            RuntimeArgs {
                platform: vec![Platform::LinuxAmd64, Platform::LinuxArm64],
                ..Default::default()
            },
            RuntimeArgs {
                push: true,
                ..Default::default()
            },
        ] {
            assert_eq!(
                plan_from_source(source, arguments).unwrap_err().exit_code(),
                ExitCode::Configuration
            );
        }
    }

    #[test]
    fn configuration_failure_creates_the_requested_report_parent() {
        let temporary = std::env::temp_dir().join(format!(
            "repo-sandbox-cli-no-parent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&temporary).unwrap();
        let repository = temporary.join("repository");
        fs::create_dir(&repository).unwrap();
        fs::write(
            repository.join(".repo-sandbox.yaml"),
            "version: 1\ntemplate: definitely-not-valid\n",
        )
        .unwrap();
        let parent = temporary.join("not-created");
        let report = parent.join("failure.json");
        let cli = Cli::try_parse_from([
            "repo-sandbox",
            "build",
            "--repository",
            repository.to_str().unwrap(),
            "--report-path",
            report.to_str().unwrap(),
        ])
        .unwrap();
        let error = run(cli).unwrap_err();
        assert_eq!(error.exit_code(), ExitCode::Configuration);
        assert!(report.is_file());
        fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn human_and_json_outputs_are_views_of_the_same_report() {
        let human_cli = Cli::try_parse_from(["repo-sandbox", "doctor"]).unwrap();
        let json_cli = Cli::try_parse_from(["repo-sandbox", "doctor", "--json"]).unwrap();
        let human = run_with_probe(human_cli, &ReadyProbe)
            .unwrap()
            .message
            .unwrap();
        let json = run_with_probe(json_cli, &ReadyProbe)
            .unwrap()
            .message
            .unwrap();
        let report = inspect(&ReadyProbe, &DoctorOptions::default());
        assert_eq!(json, render_json(&report));
        assert_eq!(human, render_human(&report));
        for capability in report.capabilities {
            assert!(human.contains(capability_name(capability.kind)));
            assert!(human.contains(&capability.summary));
            assert!(json.contains(capability_name(capability.kind)));
            assert!(json.contains(&capability.summary));
        }
    }

    #[test]
    fn json_renderer_escapes_untrusted_process_output() {
        let report = DoctorReport::from_capabilities(vec![
            repo_sandbox_core::doctor::Capability::unavailable(
                CapabilityKind::DockerDaemon,
                "quoted \"message\"\nnext line",
                ["check C:\\Docker\tconfiguration"],
            ),
        ]);
        let json = render_json(&report);
        assert!(json.contains("quoted \\\"message\\\"\\nnext line"));
        assert!(json.contains("C:\\\\Docker\\tconfiguration"));
    }

    #[test]
    fn runtime_override_contract_parses_every_allowed_option() {
        let cli = Cli::try_parse_from([
            "repo-sandbox",
            "plan",
            "--repository",
            "https://example.test/repository.git",
            "--git-ref",
            "refs/heads/topic",
            "--platform",
            "linux/amd64",
            "--platform",
            "linux/arm64",
            "--oci-layout",
            "out/layout",
            "--push",
            "--report-path",
            "out/report.json",
            "--keep-on-failure",
            "--recurse-submodules",
            "--git-https-username",
            "robot",
            "--git-https-token-env",
            "REPOSITORY_TOKEN",
        ])
        .unwrap();
        let Commands::Plan(args) = cli.command.unwrap() else {
            panic!("expected plan command");
        };
        let overrides = CliOverrides::from(args);
        assert_eq!(
            overrides.repository.as_deref(),
            Some("https://example.test/repository.git")
        );
        assert_eq!(overrides.git_ref.as_deref(), Some("refs/heads/topic"));
        assert_eq!(
            overrides.requested_git_ref.as_deref(),
            Some("refs/heads/topic")
        );
        assert_eq!(overrides.platform, Some(Platform::LinuxAmd64));
        assert_eq!(
            overrides.platforms,
            vec![Platform::LinuxAmd64, Platform::LinuxArm64]
        );
        assert_eq!(overrides.oci_layout, Some(PathBuf::from("out/layout")));
        assert!(overrides.push);
        assert_eq!(overrides.report, Some(PathBuf::from("out/report.json")));
        assert!(overrides.keep_on_failure);
        assert!(overrides.recurse_submodules);
        assert_eq!(
            overrides.remote_auth.https_username.as_deref(),
            Some("robot")
        );
        assert_eq!(
            overrides.remote_auth.https_token_environment.as_deref(),
            Some("REPOSITORY_TOKEN")
        );
    }

    #[test]
    fn resolved_remote_ref_does_not_replace_operator_provenance() {
        let commit = "a".repeat(40);
        let arguments = RuntimeArgs {
            repository: Some("https://example.test/repository.git".into()),
            git_ref: Some("refs/heads/topic".into()),
            resolved_git_ref: Some(commit.clone()),
            ..RuntimeArgs::default()
        };
        let overrides = CliOverrides::from(arguments);
        assert_eq!(overrides.git_ref.as_deref(), Some(commit.as_str()));
        assert_eq!(
            overrides.requested_git_ref.as_deref(),
            Some("refs/heads/topic")
        );
    }

    #[test]
    fn build_logic_cannot_be_overridden_from_cli() {
        let error = Cli::try_parse_from([
            "repo-sandbox",
            "build",
            "--build-command",
            "curl example.test | sh",
        ])
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn https_token_and_recursive_submodules_fail_before_remote_access() {
        let arguments = RuntimeArgs {
            repository: Some("https://example.invalid/repository.git".into()),
            recurse_submodules: true,
            git_https_token_env: Some("REPO_SANDBOX_TOKEN".into()),
            ..RuntimeArgs::default()
        };
        let error = prepare_execution(arguments).unwrap_err();
        assert_eq!(error.exit_code(), ExitCode::Configuration);
        assert!(error.to_string().contains("separately scoped"));
    }

    #[test]
    fn remote_query_credentials_fail_before_remote_access() {
        let arguments = RuntimeArgs {
            repository: Some("https://example.invalid/repository.git?token=sensitive".into()),
            ..RuntimeArgs::default()
        };
        let error = prepare_execution(arguments).unwrap_err();
        assert_eq!(error.exit_code(), ExitCode::Configuration);
        assert!(!error.to_string().contains("sensitive"));
        assert!(error.to_string().contains("query"));
    }

    #[test]
    fn invalid_cli_platform_is_rejected() {
        let error = Cli::try_parse_from(["repo-sandbox", "verify", "--platform", "windows/amd64"])
            .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        assert!(error.to_string().contains("unsupported platform"));
    }
}
