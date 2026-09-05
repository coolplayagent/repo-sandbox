//! Local Docker adapter for bounded, one-shot build and test jobs.

use crate::artifacts::{export_declared_artifacts, validate_artifact_path};
use crate::buildkit::{ProcessInvocation, ProcessOutput};
use crate::cancellation::is_cancelled;
use repo_sandbox_core::runner::{
    CleanupResult, ResourceLimit, RunReport, RunSpec, RunStatus, StepPhase, StepResult, StepStatus,
};
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const TASK_LABEL: &str = "io.repo-sandbox.task-id";
pub const REPOSITORY_LABEL: &str = "io.repo-sandbox.repository-id";
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

fn docker_container_absent(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("no such container")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockReading {
    pub unix_ms: u64,
    pub monotonic_ms: u64,
}

pub trait Clock {
    fn now(&self) -> ClockReading;
}

#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> ClockReading {
        ClockReading {
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            monotonic_ms: self
                .origin
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        }
    }
}

pub trait DockerExecutor {
    fn execute(
        &self,
        invocation: &ProcessInvocation,
        timeout: Duration,
    ) -> io::Result<ProcessOutput>;

    fn execute_streaming(
        &self,
        invocation: &ProcessInvocation,
        timeout: Duration,
        sink: &dyn LogSink,
        phase: StepPhase,
        step: &str,
    ) -> io::Result<StreamedProcessOutput> {
        let output = self.execute(invocation, timeout)?;
        let stdout_bytes = output.stdout.as_bytes().to_vec();
        let stderr_bytes = output.stderr.as_bytes().to_vec();
        sink.stdout(phase, step, &stdout_bytes);
        sink.stderr(phase, step, &stderr_bytes);
        Ok(StreamedProcessOutput {
            output,
            stdout_bytes,
            stderr_bytes,
        })
    }

    /// Execute bounded task cleanup after a cancellation has already been consumed.
    /// Implementations backed by a process-wide signal flag must not immediately
    /// cancel this operation, or Ctrl-C would strand the owned container.
    fn execute_cleanup(
        &self,
        invocation: &ProcessInvocation,
        timeout: Duration,
    ) -> io::Result<ProcessOutput> {
        self.execute(invocation, timeout)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamedProcessOutput {
    pub output: ProcessOutput,
    pub stdout_bytes: Vec<u8>,
    pub stderr_bytes: Vec<u8>,
}

/// Receives exactly the bytes captured in each [`StepResult::stdout_bytes`] and
/// [`StepResult::stderr_bytes`]. Text fields remain a readable lossy view.
pub trait LogSink: Sync {
    fn stdout(&self, phase: StepPhase, step: &str, bytes: &[u8]);
    fn stderr(&self, phase: StepPhase, step: &str, bytes: &[u8]);
}

struct RedactingLogSink<'a> {
    inner: &'a dyn LogSink,
    secrets: Vec<Vec<u8>>,
    stdout: Mutex<RedactingStream>,
    stderr: Mutex<RedactingStream>,
}

#[derive(Default)]
struct RedactingStream {
    pending: Vec<u8>,
}

impl<'a> RedactingLogSink<'a> {
    fn new(inner: &'a dyn LogSink, secrets: Vec<Vec<u8>>) -> Self {
        let mut secrets = secrets;
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        Self {
            inner,
            secrets,
            stdout: Mutex::new(RedactingStream::default()),
            stderr: Mutex::new(RedactingStream::default()),
        }
    }

    fn finish(&self, phase: StepPhase, step: &str) {
        self.flush_stream(&self.stdout, true, phase, step);
        self.flush_stream(&self.stderr, false, phase, step);
    }

    fn flush_stream(
        &self,
        stream: &Mutex<RedactingStream>,
        stdout: bool,
        phase: StepPhase,
        step: &str,
    ) {
        if let Ok(mut stream) = stream.lock() {
            let bytes = take_redacted(&mut stream.pending, &self.secrets, true);
            if stdout {
                self.inner.stdout(phase, step, &bytes);
            } else {
                self.inner.stderr(phase, step, &bytes);
            }
        }
    }

    fn emit(
        &self,
        stream: &Mutex<RedactingStream>,
        stdout: bool,
        phase: StepPhase,
        step: &str,
        bytes: &[u8],
    ) {
        let Ok(mut stream) = stream.lock() else {
            return;
        };
        stream.pending.extend_from_slice(bytes);
        let safe = take_redacted(&mut stream.pending, &self.secrets, false);
        drop(stream);
        if safe.is_empty() {
            return;
        }
        if stdout {
            self.inner.stdout(phase, step, &safe);
        } else {
            self.inner.stderr(phase, step, &safe);
        }
    }
}

impl LogSink for RedactingLogSink<'_> {
    fn stdout(&self, phase: StepPhase, step: &str, bytes: &[u8]) {
        self.emit(&self.stdout, true, phase, step, bytes);
    }

    fn stderr(&self, phase: StepPhase, step: &str, bytes: &[u8]) {
        self.emit(&self.stderr, false, phase, step, bytes);
    }
}

fn redact_bytes(bytes: &[u8], secrets: &[Vec<u8>]) -> Vec<u8> {
    let mut pending = bytes.to_vec();
    take_redacted(&mut pending, secrets, true)
}

fn take_redacted(pending: &mut Vec<u8>, secrets: &[Vec<u8>], finish: bool) -> Vec<u8> {
    let retain = if finish {
        0
    } else {
        secrets
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(1)
            .saturating_sub(1)
    };
    let limit = pending.len().saturating_sub(retain);
    let mut redacted = Vec::with_capacity(pending.len());
    let mut index = 0;
    while index < limit {
        if let Some(secret) = secrets
            .iter()
            .filter(|secret| !secret.is_empty())
            .filter(|secret| pending[index..].starts_with(secret))
            .max_by_key(|secret| secret.len())
        {
            redacted.extend_from_slice(b"[REDACTED]");
            index += secret.len();
        } else {
            redacted.push(pending[index]);
            index += 1;
        }
    }
    pending.drain(..index);
    redacted
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ConsoleLogSink;

impl LogSink for ConsoleLogSink {
    fn stdout(&self, _phase: StepPhase, _step: &str, bytes: &[u8]) {
        let _ = io::stdout().lock().write_all(bytes);
        let _ = io::stdout().lock().flush();
    }

    fn stderr(&self, _phase: StepPhase, _step: &str, bytes: &[u8]) {
        let _ = io::stderr().lock().write_all(bytes);
        let _ = io::stderr().lock().flush();
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemDockerExecutor;

impl DockerExecutor for SystemDockerExecutor {
    fn execute(
        &self,
        invocation: &ProcessInvocation,
        timeout: Duration,
    ) -> io::Result<ProcessOutput> {
        self.execute_process(invocation, timeout, None)
            .map(|output| output.output)
    }

    fn execute_streaming(
        &self,
        invocation: &ProcessInvocation,
        timeout: Duration,
        sink: &dyn LogSink,
        phase: StepPhase,
        step: &str,
    ) -> io::Result<StreamedProcessOutput> {
        self.execute_process(invocation, timeout, Some((sink, phase, step)))
    }

    fn execute_cleanup(
        &self,
        invocation: &ProcessInvocation,
        timeout: Duration,
    ) -> io::Result<ProcessOutput> {
        self.execute_process_with_cancellation(invocation, timeout, None, false)
            .map(|output| output.output)
    }
}

impl SystemDockerExecutor {
    fn execute_process(
        &self,
        invocation: &ProcessInvocation,
        timeout: Duration,
        live: Option<(&dyn LogSink, StepPhase, &str)>,
    ) -> io::Result<StreamedProcessOutput> {
        self.execute_process_with_cancellation(invocation, timeout, live, true)
    }

    fn execute_process_with_cancellation(
        &self,
        invocation: &ProcessInvocation,
        timeout: Duration,
        live: Option<(&dyn LogSink, StepPhase, &str)>,
        observe_user_cancellation: bool,
    ) -> io::Result<StreamedProcessOutput> {
        let mut command = Command::new(&invocation.program);
        command
            .args(&invocation.args)
            .current_dir(invocation.current_dir.as_deref().unwrap_or(Path::new(".")))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::snapshot::configure_process_tree(&mut command);
        let mut child = command.spawn()?;
        let process_tree = crate::snapshot::ProcessTree::attach(&mut child).inspect_err(|_| {
            let _ = child.kill();
            let _ = child.wait();
        })?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        thread::scope(|scope| {
            let stdout_reader = scope.spawn(|| {
                read_stream(stdout, |bytes| {
                    if let Some((sink, phase, step)) = live {
                        sink.stdout(phase, step, bytes);
                    }
                })
            });
            let stderr_reader = scope.spawn(|| {
                read_stream(stderr, |bytes| {
                    if let Some((sink, phase, step)) = live {
                        sink.stderr(phase, step, bytes);
                    }
                })
            });
            let deadline = Instant::now() + timeout;
            let (status, interrupted) = loop {
                if Instant::now() >= deadline || (observe_user_cancellation && is_cancelled()) {
                    process_tree.terminate();
                    break (child.wait()?, true);
                }
                if let Some(status) = child.try_wait()? {
                    break (status, false);
                }
                thread::sleep(Duration::from_millis(20));
            };
            process_tree.terminate();
            let stdout_bytes = join_scoped(stdout_reader)?;
            let stderr_bytes = join_scoped(stderr_reader)?;
            Ok(StreamedProcessOutput {
                output: ProcessOutput {
                    exit_code: status.code(),
                    stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
                    stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
                    interrupted,
                },
                stdout_bytes,
                stderr_bytes,
            })
        })
    }
}

fn read_stream(mut stream: impl Read, mut emit: impl FnMut(&[u8])) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        emit(&chunk[..count]);
        bytes.extend_from_slice(&chunk[..count]);
    }
    Ok(bytes)
}

fn join_scoped(reader: thread::ScopedJoinHandle<'_, io::Result<Vec<u8>>>) -> io::Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| io::Error::other("process output reader panicked"))?
}

fn interrupted_run_status(phase: Option<StepPhase>, step: Option<&str>) -> RunStatus {
    if is_cancelled() {
        RunStatus::Cancelled {
            phase,
            step: step.map(str::to_owned),
        }
    } else {
        RunStatus::TimedOut
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DockerRunPlan {
    pub task_label: String,
    pub container_name: String,
    pub ownership_check: ProcessInvocation,
    pub create: ProcessInvocation,
    pub start: ProcessInvocation,
    pub steps: Vec<PlannedStep>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedStep {
    pub phase: StepPhase,
    pub name: String,
    pub invocation: ProcessInvocation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanError(String);

impl Display for PlanError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for PlanError {}

pub fn plan(spec: &RunSpec) -> Result<DockerRunPlan, PlanError> {
    validate_spec(spec)?;
    let label = format!("{TASK_LABEL}={}", spec.task_id);
    let repository_label = format!("{REPOSITORY_LABEL}={}", spec.repository_id);
    let name = format!("repo-sandbox-{}", spec.task_id);
    let ownership_check = docker(vec![
        "container",
        "ls",
        "--all",
        "--quiet",
        "--filter",
        &format!("label={label}"),
    ]);
    let memory = format!("{}m", spec.resources.memory_mb);
    let cpu = spec.resources.cpu_count.to_string();
    let temporary = format!(
        "/tmp:rw,nosuid,nodev,size={}m",
        spec.resources.temporary_storage_mb
    );
    let writable_layer = format!("size={}m", spec.resources.temporary_storage_mb);
    let mut create_args = vec![
        "container",
        "create",
        "--name",
        &name,
        "--label",
        &label,
        "--label",
        &repository_label,
        "--network",
        "none",
        "--cpus",
        &cpu,
        "--memory",
        &memory,
        "--memory-swap",
        &memory,
        "--tmpfs",
        &temporary,
        "--storage-opt",
        &writable_layer,
        "--security-opt",
        "no-new-privileges=true",
        "--cap-drop",
        "ALL",
        "--workdir",
        "/workspace",
        "--platform",
        spec.platform.as_str(),
    ];
    for name in &spec.environment_names {
        create_args.extend(["--env", name]);
    }
    let secret_mounts = spec
        .secret_mounts
        .iter()
        .map(|secret| {
            format!(
                "type=bind,src={},dst=/run/repo-sandbox-secrets/{},readonly",
                secret.source.to_string_lossy(),
                secret.environment
            )
        })
        .collect::<Vec<_>>();
    for mount in &secret_mounts {
        create_args.extend(["--mount", mount.as_str()]);
    }
    create_args.extend([
        spec.image.as_str(),
        "/bin/sh",
        "-c",
        "trap 'exit 0' TERM INT; while :; do sleep 3600; done",
    ]);
    let create = docker(create_args);
    let start = docker(vec!["container", "start", &name]);
    let steps = spec
        .build
        .iter()
        .map(|step| (StepPhase::Build, step))
        .chain(spec.test.iter().map(|step| (StepPhase::Test, step)))
        .map(|(phase, step)| PlannedStep {
            phase,
            name: step.name.clone(),
            invocation: docker(vec![
                "container",
                "exec",
                &name,
                "/bin/sh",
                "-c",
                &if spec.secret_mounts.is_empty() { step.command.clone() } else {
                    format!("for f in /run/repo-sandbox-secrets/*; do n=${{f##*/}}; v=$(cat \"$f\" && printf .) || exit 125; v=${{v%.}}; export \"$n=$v\"; done; {}", step.command)
                },
            ]),
        })
        .collect();
    Ok(DockerRunPlan {
        task_label: label,
        container_name: name,
        ownership_check,
        create,
        start,
        steps,
    })
}

fn docker(args: Vec<&str>) -> ProcessInvocation {
    ProcessInvocation {
        program: "docker".to_owned(),
        args: args.into_iter().map(str::to_owned).collect(),
        current_dir: None,
    }
}

fn validate_spec(spec: &RunSpec) -> Result<(), PlanError> {
    let valid_task = !spec.task_id.is_empty()
        && spec.task_id.len() <= 48
        && spec.task_id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        });
    if !valid_task {
        return Err(PlanError(
            "task_id must be 1..=48 lowercase ASCII letters, digits, '-' or '_'".to_owned(),
        ));
    }
    if !spec.repository_id.starts_with("sha256:")
        || spec.repository_id.len() != 71
        || !spec.repository_id[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(PlanError(
            "repository_id must be a normalized sha256 digest".to_owned(),
        ));
    }
    if spec.timeout_ms == 0 {
        return Err(PlanError("timeout_ms must be greater than zero".to_owned()));
    }
    if spec.resources.cpu_count == 0
        || spec.resources.memory_mb == 0
        || spec.resources.temporary_storage_mb == 0
    {
        return Err(PlanError(
            "all resource limits must be greater than zero".to_owned(),
        ));
    }
    if spec.build.is_empty() && spec.test.is_empty() {
        return Err(PlanError(
            "at least one build or test step is required".to_owned(),
        ));
    }
    if spec
        .build
        .iter()
        .chain(&spec.test)
        .any(|step| step.name.trim().is_empty() || step.command.trim().is_empty())
    {
        return Err(PlanError(
            "step names and commands must not be empty".to_owned(),
        ));
    }
    if spec.environment_names.iter().any(|name| {
        name.is_empty()
            || name.bytes().enumerate().any(|(index, byte)| {
                !(byte == b'_'
                    || byte.is_ascii_alphabetic()
                    || (index > 0 && byte.is_ascii_digit()))
            })
    }) {
        return Err(PlanError(
            "environment names must use POSIX identifier syntax".to_owned(),
        ));
    }
    if spec.secret_mounts.iter().any(|secret| {
        secret.environment.is_empty()
            || !secret.source.is_file()
            || secret.environment.bytes().enumerate().any(|(index, byte)| {
                !(byte == b'_'
                    || byte.is_ascii_alphabetic()
                    || (index > 0 && byte.is_ascii_digit()))
            })
    }) {
        return Err(PlanError(
            "secret mounts require a regular file and POSIX environment name".to_owned(),
        ));
    }
    Ok(())
}

pub struct DockerRunner<E, C, S = ConsoleLogSink> {
    executor: E,
    clock: C,
    sink: S,
}

impl<E, C> DockerRunner<E, C, ConsoleLogSink> {
    pub const fn new(executor: E, clock: C) -> Self {
        Self {
            executor,
            clock,
            sink: ConsoleLogSink,
        }
    }
}

impl<E, C, S> DockerRunner<E, C, S> {
    pub const fn new_with_sink(executor: E, clock: C, sink: S) -> Self {
        Self {
            executor,
            clock,
            sink,
        }
    }
}

impl<E: DockerExecutor, C: Clock, S: LogSink> DockerRunner<E, C, S> {
    pub fn run(&self, spec: &RunSpec) -> Result<RunReport, PlanError> {
        self.run_with_container_hook(spec, |_| Ok(()))
    }

    /// Register the exact Docker ID immediately after successful creation and
    /// before start. A failed durable registration triggers exact cleanup.
    pub fn run_with_container_hook(
        &self,
        spec: &RunSpec,
        hook: impl FnOnce(&str) -> Result<(), String>,
    ) -> Result<RunReport, PlanError> {
        let run_plan = plan(spec)?;
        let started = self.clock.now();
        let mut report = RunReport {
            schema_version: 1,
            plan_digest: spec.config_summary.plan_digest.clone(),
            phase: "runner".into(),
            exit_code: 0,
            message: String::new(),
            task_id: spec.task_id.clone(),
            container_id: None,
            source_snapshot: spec.source_snapshot.clone(),
            config: spec.config_summary.clone(),
            image: spec.image.clone(),
            image_digest: spec.image_digest.clone(),
            started_at_unix_ms: started.unix_ms,
            ended_at_unix_ms: started.unix_ms,
            duration_ms: 0,
            status: RunStatus::Succeeded,
            steps: Vec::new(),
            exported_artifacts: Vec::new(),
            artifact_error: None,
            cleanup: CleanupResult::NotNeeded,
            cleanup_error: None,
            published: None,
            publication_progress: Vec::new(),
        };

        let platform_inspect = docker(vec![
            "image",
            "inspect",
            "--format",
            "{{.Os}}/{{.Architecture}}",
            spec.image.as_str(),
        ]);
        match self.execute_remaining(&platform_inspect, spec, started.monotonic_ms) {
            Ok(output) if output.interrupted => {
                report.status = interrupted_run_status(None, None);
                return Ok(self.finish(report, started));
            }
            Ok(output)
                if output.exit_code == Some(0)
                    && output.stdout.trim() == spec.platform.as_str() => {}
            Ok(output) => {
                report.status = infrastructure(
                    "validate runner platform",
                    format!(
                        "expected {}, inspected {}: {}",
                        spec.platform.as_str(),
                        output.stdout.trim(),
                        output.stderr.trim()
                    ),
                );
                return Ok(self.finish(report, started));
            }
            Err(error) => {
                report.status = infrastructure("validate runner platform", error.to_string());
                return Ok(self.finish(report, started));
            }
        }
        if let Err(status) = self.ensure_unowned(&run_plan, spec, started.monotonic_ms) {
            report.status = status;
            return Ok(self.finish(report, started));
        }
        let create = match self.execute_remaining(&run_plan.create, spec, started.monotonic_ms) {
            Ok(output) if output.interrupted => {
                report.status = interrupted_run_status(None, None);
                self.reconcile_interrupted_create(&run_plan, spec, &mut report, hook);
                return Ok(self.finish(report, started));
            }
            Ok(output) if output.exit_code == Some(0) => output,
            Ok(output) => {
                report.status = infrastructure("create owned container", output.stderr);
                return Ok(self.finish(report, started));
            }
            Err(error) => {
                report.status = infrastructure("create owned container", error.to_string());
                return Ok(self.finish(report, started));
            }
        };
        let container_id = create.stdout.trim().to_owned();
        if container_id.is_empty() {
            report.status =
                infrastructure("create owned container", "Docker returned no container ID");
            return Ok(self.finish(report, started));
        }
        report.container_id = Some(container_id.clone());
        if let Err(error) = hook(&container_id) {
            report.status = infrastructure("register owned container", error);
            match self.cleanup(&container_id) {
                Ok(()) => report.cleanup = CleanupResult::Removed,
                Err(error) => {
                    report.cleanup = CleanupResult::Failed;
                    report.cleanup_error = Some(error.to_string());
                }
            }
            return Ok(self.finish(report, started));
        }

        match self.execute_remaining(&run_plan.start, spec, started.monotonic_ms) {
            Ok(output) if output.interrupted => report.status = interrupted_run_status(None, None),
            Ok(output) if output.exit_code != Some(0) => {
                report.status = infrastructure("start owned container", output.stderr)
            }
            Err(error) => {
                report.status = infrastructure("start owned container", error.to_string())
            }
            Ok(_) => self.run_steps(spec, &run_plan, started, &container_id, &mut report),
        }

        if let Some(export_root) = &spec.artifact_export_root
            && !report.steps.is_empty()
            && !spec.config_summary.artifact_directories.is_empty()
        {
            match self.export_artifacts(
                &container_id,
                &spec.config_summary.artifact_directories,
                export_root,
            ) {
                Ok(paths) => report.exported_artifacts = paths,
                Err(error) => {
                    report.artifact_error = Some(error.clone());
                    if report.status == RunStatus::Succeeded {
                        report.status = infrastructure("export declared artifacts", error);
                    }
                }
            }
        }

        if spec.keep_on_failure && report.status != RunStatus::Succeeded {
            report.cleanup = CleanupResult::RetainedOnFailure;
        } else {
            match self.cleanup(&container_id) {
                Ok(()) => report.cleanup = CleanupResult::Removed,
                Err(error) => {
                    report.cleanup = CleanupResult::Failed;
                    report.cleanup_error = Some(error.to_string());
                }
            }
        }
        Ok(self.finish(report, started))
    }

    fn reconcile_interrupted_create(
        &self,
        run_plan: &DockerRunPlan,
        spec: &RunSpec,
        report: &mut RunReport,
        hook: impl FnOnce(&str) -> Result<(), String>,
    ) {
        let inspect = docker(vec![
            "container",
            "inspect",
            "--format",
            "{{.Id}}|{{ index .Config.Labels \"io.repo-sandbox.task-id\" }}|{{ index .Config.Labels \"io.repo-sandbox.repository-id\" }}",
            &run_plan.container_name,
        ]);
        let deadline = Instant::now() + CLEANUP_TIMEOUT;
        let mut absence_observed = false;
        let output = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if absence_observed {
                    return;
                }
                report.cleanup = CleanupResult::Failed;
                report.cleanup_error =
                    Some("reconcile interrupted container creation timed out".into());
                return;
            }
            match self.executor.execute_cleanup(&inspect, remaining) {
                Ok(output) if output.exit_code == Some(0) => break output,
                Ok(output) if docker_container_absent(&output.stderr) => {
                    absence_observed = true;
                    thread::sleep(Duration::from_millis(50));
                }
                Ok(output) => {
                    report.cleanup = CleanupResult::Failed;
                    report.cleanup_error = Some(format!(
                        "reconcile interrupted container creation: {}",
                        output.stderr.trim()
                    ));
                    return;
                }
                Err(error) => {
                    report.cleanup = CleanupResult::Failed;
                    report.cleanup_error =
                        Some(format!("reconcile interrupted container creation: {error}"));
                    return;
                }
            }
        };
        let mut fields = output.stdout.trim().split('|');
        let id = fields.next().unwrap_or_default();
        let task = fields.next().unwrap_or_default();
        let repository = fields.next().unwrap_or_default();
        if id.is_empty() || task != spec.task_id || repository != spec.repository_id {
            report.cleanup = CleanupResult::Failed;
            report.cleanup_error = Some(
                "refused to remove interrupted container because ownership labels do not match"
                    .into(),
            );
            return;
        }
        report.container_id = Some(id.into());
        if let Err(error) = hook(id) {
            report.cleanup_error = Some(format!("register interrupted owned container: {error}"));
        }
        match self.cleanup(id) {
            Ok(()) => report.cleanup = CleanupResult::Removed,
            Err(error) => {
                report.cleanup = CleanupResult::Failed;
                report.cleanup_error = Some(match report.cleanup_error.take() {
                    Some(primary) => format!("{primary}; {error}"),
                    None => error.to_string(),
                });
            }
        }
    }

    fn ensure_unowned(
        &self,
        run_plan: &DockerRunPlan,
        spec: &RunSpec,
        started_ms: u64,
    ) -> Result<(), RunStatus> {
        match self.execute_remaining(&run_plan.ownership_check, spec, started_ms) {
            Ok(output) if output.interrupted => Err(interrupted_run_status(None, None)),
            Ok(output) if output.exit_code != Some(0) => {
                Err(infrastructure("check task ownership label", output.stderr))
            }
            Ok(output) if !output.stdout.trim().is_empty() => Err(infrastructure(
                "check task ownership label",
                "a container already exists with this task label",
            )),
            Ok(_) => Ok(()),
            Err(error) => Err(infrastructure(
                "check task ownership label",
                error.to_string(),
            )),
        }
    }

    fn run_steps(
        &self,
        spec: &RunSpec,
        run_plan: &DockerRunPlan,
        started: ClockReading,
        container_id: &str,
        report: &mut RunReport,
    ) {
        let mut first_command_failure = None;
        for step in &run_plan.steps {
            let step_started = self.clock.now();
            let output = self.execute_step_remaining(
                &step.invocation,
                spec,
                started.monotonic_ms,
                step.phase,
                &step.name,
            );
            let step_ended = self.clock.now();
            let (status, exit_code, stdout, stderr, stdout_bytes, stderr_bytes) = match output {
                Err(error) => (
                    StepStatus::InfrastructureFailed,
                    None,
                    String::new(),
                    error.to_string(),
                    Vec::new(),
                    Vec::new(),
                ),
                Ok(streamed) if streamed.output.interrupted => (
                    if is_cancelled() {
                        StepStatus::Cancelled
                    } else {
                        StepStatus::TimedOut
                    },
                    streamed.output.exit_code,
                    streamed.output.stdout,
                    streamed.output.stderr,
                    streamed.stdout_bytes,
                    streamed.stderr_bytes,
                ),
                Ok(streamed) if streamed.output.exit_code == Some(0) => (
                    StepStatus::Succeeded,
                    streamed.output.exit_code,
                    streamed.output.stdout,
                    streamed.output.stderr,
                    streamed.stdout_bytes,
                    streamed.stderr_bytes,
                ),
                Ok(streamed) => {
                    let limit = self.resource_limit(container_id, &streamed.output);
                    let status = limit.map_or(StepStatus::CommandFailed, |limit| {
                        StepStatus::ResourceExceeded { limit }
                    });
                    (
                        status,
                        streamed.output.exit_code,
                        streamed.output.stdout,
                        streamed.output.stderr,
                        streamed.stdout_bytes,
                        streamed.stderr_bytes,
                    )
                }
            };
            report.steps.push(StepResult {
                phase: step.phase,
                name: step.name.clone(),
                started_at_unix_ms: step_started.unix_ms,
                ended_at_unix_ms: step_ended.unix_ms,
                duration_ms: step_ended
                    .monotonic_ms
                    .saturating_sub(step_started.monotonic_ms),
                exit_code,
                status: status.clone(),
                stdout,
                stderr: stderr.clone(),
                stdout_bytes,
                stderr_bytes,
            });

            match status {
                StepStatus::Succeeded => {}
                StepStatus::CommandFailed => {
                    let failure = RunStatus::CommandFailed {
                        phase: step.phase,
                        step: step.name.clone(),
                        exit_code,
                    };
                    if first_command_failure.is_none() {
                        first_command_failure = Some(failure.clone());
                    }
                    if spec.fail_fast {
                        report.status = failure;
                        return;
                    }
                }
                StepStatus::TimedOut => {
                    report.status = RunStatus::TimedOut;
                    return;
                }
                StepStatus::Cancelled => {
                    report.status = interrupted_run_status(Some(step.phase), Some(&step.name));
                    return;
                }
                StepStatus::ResourceExceeded { limit } => {
                    report.status = RunStatus::ResourceExceeded {
                        phase: step.phase,
                        step: step.name.clone(),
                        limit,
                    };
                    return;
                }
                StepStatus::InfrastructureFailed => {
                    report.status = infrastructure("execute job step", stderr);
                    return;
                }
            }
        }
        if let Some(failure) = first_command_failure {
            report.status = failure;
        }
    }

    fn resource_limit(&self, container_id: &str, output: &ProcessOutput) -> Option<ResourceLimit> {
        let no_space = output
            .stderr
            .to_ascii_lowercase()
            .contains("no space left on device");
        if no_space {
            return Some(ResourceLimit::TemporaryStorage);
        }
        if output.exit_code != Some(137) {
            return None;
        }
        let inspect = docker(vec![
            "container",
            "inspect",
            "--format",
            "{{.State.OOMKilled}}",
            container_id,
        ]);
        self.executor
            .execute(&inspect, CLEANUP_TIMEOUT)
            .ok()
            .filter(|result| result.exit_code == Some(0))
            .filter(|result| result.stdout.trim().eq_ignore_ascii_case("true"))
            .map(|_| ResourceLimit::Memory)
    }

    fn execute_remaining(
        &self,
        invocation: &ProcessInvocation,
        spec: &RunSpec,
        started_ms: u64,
    ) -> io::Result<ProcessOutput> {
        let elapsed = self.clock.now().monotonic_ms.saturating_sub(started_ms);
        let remaining = spec.timeout_ms.saturating_sub(elapsed);
        if remaining == 0 {
            return Ok(ProcessOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: "total job timeout elapsed".to_owned(),
                interrupted: true,
            });
        }
        self.executor
            .execute(invocation, Duration::from_millis(remaining))
    }

    fn execute_step_remaining(
        &self,
        invocation: &ProcessInvocation,
        spec: &RunSpec,
        started_ms: u64,
        phase: StepPhase,
        step: &str,
    ) -> io::Result<StreamedProcessOutput> {
        let elapsed = self.clock.now().monotonic_ms.saturating_sub(started_ms);
        let remaining = spec.timeout_ms.saturating_sub(elapsed);
        if remaining == 0 {
            return Ok(StreamedProcessOutput {
                output: ProcessOutput {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: "total job timeout elapsed".to_owned(),
                    interrupted: true,
                },
                stdout_bytes: Vec::new(),
                stderr_bytes: Vec::new(),
            });
        }
        let secrets = load_redaction_secrets(&spec.secret_mounts)?;
        if secrets.is_empty() {
            return self.executor.execute_streaming(
                invocation,
                Duration::from_millis(remaining),
                &self.sink,
                phase,
                step,
            );
        }
        let sink = RedactingLogSink::new(&self.sink, secrets.clone());
        let streamed = self.executor.execute_streaming(
            invocation,
            Duration::from_millis(remaining),
            &sink,
            phase,
            step,
        );
        sink.finish(phase, step);
        let mut streamed = streamed?;
        streamed.stdout_bytes = redact_bytes(&streamed.stdout_bytes, &secrets);
        streamed.stderr_bytes = redact_bytes(&streamed.stderr_bytes, &secrets);
        streamed.output.stdout = String::from_utf8_lossy(&streamed.stdout_bytes).into_owned();
        streamed.output.stderr = String::from_utf8_lossy(&streamed.stderr_bytes).into_owned();
        Ok(streamed)
    }

    fn cleanup(&self, container_id: &str) -> io::Result<()> {
        let invocation = docker(vec!["container", "rm", "--force", container_id]);
        let output = self
            .executor
            .execute_cleanup(&invocation, CLEANUP_TIMEOUT)?;
        if output.exit_code == Some(0) {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "remove owned container `{container_id}`: {}",
                output.stderr.trim()
            )))
        }
    }

    fn export_artifacts(
        &self,
        container_id: &str,
        declared: &[std::path::PathBuf],
        export_root: &Path,
    ) -> Result<Vec<std::path::PathBuf>, String> {
        for path in declared {
            validate_artifact_path(path).map_err(|error| error.to_string())?;
        }
        let staging = tempfile::Builder::new()
            .prefix("repo-sandbox-artifacts-")
            .tempdir()
            .map_err(|error| error.to_string())?;
        for path in declared {
            let parent = path.parent().unwrap_or_else(|| Path::new(""));
            std::fs::create_dir_all(staging.path().join(parent))
                .map_err(|error| error.to_string())?;
            let container_path = path.to_string_lossy().replace('\\', "/");
            let destination = staging.path().join(parent);
            let invocation = docker(vec![
                "container",
                "cp",
                &format!("{container_id}:/workspace/{container_path}"),
                &destination.to_string_lossy(),
            ]);
            let output = self
                .executor
                .execute(&invocation, CLEANUP_TIMEOUT)
                .map_err(|error| error.to_string())?;
            if output.exit_code != Some(0) {
                return Err(format!(
                    "copy declared artifact `{}` from owned container: {}",
                    path.display(),
                    output.stderr.trim()
                ));
            }
        }
        export_declared_artifacts(staging.path(), declared, declared, export_root)
            .map_err(|error| error.to_string())
    }

    fn finish(&self, mut report: RunReport, started: ClockReading) -> RunReport {
        let ended = self.clock.now();
        report.ended_at_unix_ms = ended.unix_ms;
        report.duration_ms = ended.monotonic_ms.saturating_sub(started.monotonic_ms);
        report
    }
}

fn load_redaction_secrets(
    mounts: &[repo_sandbox_core::runner::SecretMount],
) -> io::Result<Vec<Vec<u8>>> {
    mounts
        .iter()
        .map(|secret| {
            std::fs::read(&secret.source).and_then(|value| {
                if value.is_empty() {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("secret redaction source is empty: {}", secret.environment),
                    ))
                } else {
                    Ok(value)
                }
            })
        })
        .collect()
}

fn infrastructure(operation: impl Into<String>, message: impl Into<String>) -> RunStatus {
    RunStatus::InfrastructureFailed {
        operation: operation.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use repo_sandbox_core::build::ImageDigest;
    use repo_sandbox_core::build::ImageRef;
    use repo_sandbox_core::config::Platform;
    use repo_sandbox_core::runner::{
        ConfigSummary, RunResources, RunStep, SecretMount, write_report_json,
    };
    use repo_sandbox_core::snapshot::{SnapshotId, SnapshotOrigin, SourceSnapshot};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::fs;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct FakeClock(Rc<Cell<u64>>);

    impl Clock for FakeClock {
        fn now(&self) -> ClockReading {
            let elapsed = self.0.get();
            ClockReading {
                unix_ms: 1_800_000_000_000 + elapsed,
                monotonic_ms: elapsed,
            }
        }
    }

    struct FakeExecutor {
        clock: Rc<Cell<u64>>,
        outputs: RefCell<VecDeque<io::Result<ProcessOutput>>>,
        invocations: RefCell<Vec<(ProcessInvocation, Duration)>>,
    }

    impl FakeExecutor {
        fn new(clock: &FakeClock, outputs: Vec<ProcessOutput>) -> Self {
            Self {
                clock: Rc::clone(&clock.0),
                outputs: RefCell::new(outputs.into_iter().map(Ok).collect()),
                invocations: RefCell::new(Vec::new()),
            }
        }

        fn invocations(&self) -> Vec<ProcessInvocation> {
            self.invocations
                .borrow()
                .iter()
                .map(|(invocation, _)| invocation.clone())
                .collect()
        }
    }

    impl DockerExecutor for &FakeExecutor {
        fn execute(
            &self,
            invocation: &ProcessInvocation,
            timeout: Duration,
        ) -> io::Result<ProcessOutput> {
            if invocation
                .args
                .starts_with(&["image".into(), "inspect".into()])
            {
                return Ok(output(0, "linux/amd64", ""));
            }
            self.invocations
                .borrow_mut()
                .push((invocation.clone(), timeout));
            self.clock.set(self.clock.get() + 10);
            self.outputs
                .borrow_mut()
                .pop_front()
                .expect("fake output for every invocation")
        }
    }

    struct RawStepExecutor {
        clock: Rc<Cell<u64>>,
        control_calls: Cell<usize>,
        bytes: Vec<u8>,
    }

    struct CleanupRoutingExecutor {
        ordinary_calls: Cell<usize>,
        cleanup_calls: Cell<usize>,
    }

    impl DockerExecutor for &CleanupRoutingExecutor {
        fn execute(
            &self,
            _invocation: &ProcessInvocation,
            _timeout: Duration,
        ) -> io::Result<ProcessOutput> {
            self.ordinary_calls.set(self.ordinary_calls.get() + 1);
            Err(io::Error::other("ordinary execution observes cancellation"))
        }

        fn execute_cleanup(
            &self,
            invocation: &ProcessInvocation,
            timeout: Duration,
        ) -> io::Result<ProcessOutput> {
            self.cleanup_calls.set(self.cleanup_calls.get() + 1);
            assert_eq!(timeout, CLEANUP_TIMEOUT);
            assert_eq!(
                invocation.args,
                ["container", "rm", "--force", "owned-container"]
            );
            Ok(output(0, "", ""))
        }
    }

    impl DockerExecutor for &RawStepExecutor {
        fn execute(
            &self,
            invocation: &ProcessInvocation,
            _timeout: Duration,
        ) -> io::Result<ProcessOutput> {
            if invocation
                .args
                .starts_with(&["image".into(), "inspect".into()])
            {
                return Ok(output(0, "linux/amd64", ""));
            }
            self.clock.set(self.clock.get() + 10);
            let call = self.control_calls.get();
            self.control_calls.set(call + 1);
            Ok(match call {
                0 => output(0, "", ""),
                1 => output(0, "container-id-raw", ""),
                _ => output(0, "", ""),
            })
        }

        fn execute_streaming(
            &self,
            _invocation: &ProcessInvocation,
            _timeout: Duration,
            sink: &dyn LogSink,
            phase: StepPhase,
            step: &str,
        ) -> io::Result<StreamedProcessOutput> {
            self.clock.set(self.clock.get() + 10);
            let split = 2.min(self.bytes.len());
            sink.stdout(phase, step, &self.bytes[..split]);
            sink.stdout(phase, step, &self.bytes[split..]);
            Ok(StreamedProcessOutput {
                output: ProcessOutput {
                    exit_code: Some(0),
                    stdout: String::from_utf8_lossy(&self.bytes).into_owned(),
                    stderr: String::new(),
                    interrupted: false,
                },
                stdout_bytes: self.bytes.clone(),
                stderr_bytes: Vec::new(),
            })
        }
    }

    fn output(exit_code: i32, stdout: &str, stderr: &str) -> ProcessOutput {
        ProcessOutput {
            exit_code: Some(exit_code),
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            interrupted: false,
        }
    }

    fn timed_out() -> ProcessOutput {
        ProcessOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            interrupted: true,
        }
    }

    fn spec(fail_fast: bool) -> RunSpec {
        RunSpec {
            task_id: "task-7".to_owned(),
            repository_id: format!("sha256:{}", "a".repeat(64)),
            image: ImageRef::new("repo-sandbox/task@sha256:abc").unwrap(),
            image_digest: ImageDigest::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
            source_snapshot: SourceSnapshot {
                id: SnapshotId::parse("b".repeat(64)).unwrap(),
                origin: SnapshotOrigin::Local {
                    canonical_root: PathBuf::from("/workspace/source"),
                },
                file_count: 7,
                recurse_submodules: false,
            },
            config_summary: ConfigSummary {
                template_id: "rust".to_owned(),
                plan_digest: "sha256:test".to_owned(),
                platform: Platform::LinuxAmd64,
                build_steps: vec!["compile".to_owned(), "lint".to_owned()],
                test_steps: vec!["unit".to_owned()],
                artifact_directories: vec![PathBuf::from("target/release")],
            },
            platform: Platform::LinuxAmd64,
            build: vec![
                RunStep {
                    name: "compile".to_owned(),
                    command: "cargo build --locked".to_owned(),
                },
                RunStep {
                    name: "lint".to_owned(),
                    command: "cargo clippy".to_owned(),
                },
            ],
            test: vec![RunStep {
                name: "unit".to_owned(),
                command: "cargo test --locked".to_owned(),
            }],
            resources: RunResources {
                cpu_count: 2,
                memory_mb: 2048,
                temporary_storage_mb: 512,
            },
            timeout_ms: 5_000,
            fail_fast,
            environment_names: Vec::new(),
            secret_mounts: Vec::new(),
            artifact_export_root: None,
            keep_on_failure: false,
        }
    }

    #[test]
    fn secret_values_never_reach_docker_argv_sink_or_report() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let executor = FakeExecutor::new(
            &clock,
            vec![
                output(0, "", ""),
                output(0, "container-secret\n", ""),
                output(0, "", ""),
                output(0, "token-super-secret\n", "err token-super-secret"),
                output(0, "", ""),
            ],
        );
        let temporary = tempfile::tempdir().unwrap();
        let secret = temporary.path().join("TOKEN");
        fs::write(&secret, "token-super-secret").unwrap();
        let mut request = spec(true);
        request.build.truncate(1);
        request.test.clear();
        request.secret_mounts = vec![SecretMount {
            environment: "TOKEN".into(),
            source: secret,
        }];
        let sink = RecordingSink::default();
        let report = DockerRunner::new_with_sink(&executor, clock, sink.clone())
            .run(&request)
            .unwrap();
        let invocations = format!("{:?}", executor.invocations());
        let json = serde_json::to_string(&report).unwrap();
        assert!(invocations.contains("printf ."));
        assert!(invocations.contains("v=${v%.}"));
        assert!(!invocations.contains("token-super-secret"));
        assert!(!json.contains("token-super-secret"));
        assert!(
            !String::from_utf8_lossy(&sink.stdout.lock().unwrap()).contains("token-super-secret")
        );
        assert!(json.contains("[REDACTED]"));
    }

    #[cfg(unix)]
    #[test]
    fn mounted_secret_loading_preserves_embedded_and_trailing_newlines() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("TOKEN");
        let secret = b"first\nsecond\n\n";
        fs::write(&source, secret).unwrap();
        let mut run = spec(true);
        run.secret_mounts = vec![SecretMount {
            environment: "TOKEN".into(),
            source,
        }];
        run.build[0].command = "printf %s \"$TOKEN\"".into();
        let script = plan(&run).unwrap().steps[0]
            .invocation
            .args
            .last()
            .unwrap()
            .replace(
                "/run/repo-sandbox-secrets",
                &directory.path().to_string_lossy(),
            );
        let result = Command::new("/bin/sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(result.status.success());
        assert_eq!(result.stdout, secret);
        assert_eq!(
            redact_bytes(&result.stdout, &[secret.to_vec()]),
            b"[REDACTED]"
        );
    }

    #[test]
    fn runner_rejects_mismatched_image_platform_before_container_creation() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let executor = FakeExecutor::new(&clock, vec![]);
        let mut run = spec(true);
        run.platform = repo_sandbox_core::config::Platform::LinuxArm64;
        let runner = DockerRunner::new(&executor, clock);
        let report = runner.run(&run).unwrap();
        assert!(matches!(
            report.status,
            RunStatus::InfrastructureFailed { .. }
        ));
        assert!(executor.invocations().is_empty());
    }

    #[test]
    fn unreadable_secret_redaction_source_fails_closed_before_step_execution() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("removed-secret");
        fs::write(&source, "secret-value").unwrap();
        let mount = SecretMount {
            environment: "TOKEN".into(),
            source: source.clone(),
        };
        fs::remove_file(source).unwrap();
        let error = load_redaction_secrets(&[mount]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!error.to_string().contains("secret-value"));
    }

    #[test]
    fn live_secret_redaction_handles_values_split_across_chunks() {
        let sink = RecordingSink::default();
        let redacting = RedactingLogSink::new(&sink, vec![b"split-secret".to_vec()]);
        redacting.stdout(StepPhase::Test, "secret", b"prefix split-");
        redacting.stdout(StepPhase::Test, "secret", b"secret suffix\n");
        redacting.finish(StepPhase::Test, "secret");
        assert_eq!(&*sink.stdout.lock().unwrap(), b"prefix [REDACTED] suffix\n");
    }

    #[test]
    fn shared_prefix_secrets_redact_the_longest_value_first() {
        let sink = RecordingSink::default();
        let redacting = RedactingLogSink::new(
            &sink,
            vec![b"token".to_vec(), b"token-with-private-suffix".to_vec()],
        );
        redacting.stdout(StepPhase::Test, "secret", b"token-with-private-suffix\n");
        redacting.finish(StepPhase::Test, "secret");
        assert_eq!(&*sink.stdout.lock().unwrap(), b"[REDACTED]\n");
    }

    #[derive(Clone, Default)]
    struct RecordingSink {
        stdout: Arc<Mutex<Vec<u8>>>,
        stderr: Arc<Mutex<Vec<u8>>>,
    }

    impl LogSink for RecordingSink {
        fn stdout(&self, _phase: StepPhase, _step: &str, bytes: &[u8]) {
            self.stdout.lock().unwrap().extend_from_slice(bytes);
        }

        fn stderr(&self, _phase: StepPhase, _step: &str, bytes: &[u8]) {
            self.stderr.lock().unwrap().extend_from_slice(bytes);
        }
    }

    fn success_outputs(step_count: usize) -> Vec<ProcessOutput> {
        let mut outputs = vec![
            output(0, "", ""),
            output(0, "container-id-7\n", ""),
            output(0, "container-id-7\n", ""),
        ];
        outputs.extend((0..step_count).map(|_| output(0, "ok", "")));
        outputs.push(output(0, "container-id-7", ""));
        outputs
    }

    #[test]
    fn plan_exposes_secure_bounded_defaults_and_structured_commands() {
        let plan = plan(&spec(true)).unwrap();
        assert_eq!(plan.task_label, "io.repo-sandbox.task-id=task-7");
        assert_eq!(plan.create.program, "docker");
        let args = &plan.create.args;
        for pair in [
            ["--network", "none"],
            ["--cpus", "2"],
            ["--memory", "2048m"],
            ["--memory-swap", "2048m"],
            ["--security-opt", "no-new-privileges=true"],
            ["--cap-drop", "ALL"],
        ] {
            assert!(args.windows(2).any(|actual| actual == pair));
        }
        assert!(plan.steps.iter().all(|step| {
            step.invocation
                .args
                .windows(2)
                .any(|args| args == ["/bin/sh", "-c"])
                && !step.invocation.args.iter().any(|arg| arg == "-lc")
        }));
        assert!(
            args.windows(2).any(|pair| {
                pair[0] == "--tmpfs" && pair[1] == "/tmp:rw,nosuid,nodev,size=512m"
            })
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--storage-opt" && pair[1] == "size=512m")
        );
        assert!(!args.iter().any(|arg| arg == "--privileged"));
        assert!(!args.iter().any(|arg| arg.contains("docker.sock")));
        assert!(
            !args
                .iter()
                .any(|arg| matches!(arg.as_str(), "-i" | "-t" | "-it"))
        );
        assert_eq!(plan.steps[0].phase, StepPhase::Build);
        assert_eq!(plan.steps[2].phase, StepPhase::Test);
        assert_eq!(
            plan.steps[0].invocation.args.last().unwrap(),
            "cargo build --locked"
        );
    }

    #[test]
    fn successful_job_records_order_timing_exit_codes_and_exact_cleanup() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let executor = FakeExecutor::new(&clock, success_outputs(3));
        let report = DockerRunner::new(&executor, clock)
            .run(&spec(true))
            .unwrap();
        assert_eq!(report.status, RunStatus::Succeeded);
        assert_eq!(report.container_id.as_deref(), Some("container-id-7"));
        assert_eq!(report.steps.len(), 3);
        assert_eq!(report.steps[0].name, "compile");
        assert_eq!(report.steps[1].name, "lint");
        assert_eq!(report.steps[2].name, "unit");
        assert!(report.steps.iter().all(|step| step.exit_code == Some(0)));
        assert!(report.steps.iter().all(|step| step.duration_ms == 10));
        assert_eq!(report.source_snapshot, spec(true).source_snapshot);
        assert_eq!(report.image_digest, spec(true).image_digest);
        assert_eq!(report.cleanup, CleanupResult::Removed);
        let calls = executor.invocations();
        assert_eq!(
            calls.last().unwrap().args,
            ["container", "rm", "--force", "container-id-7"]
        );
        assert!(
            calls
                .iter()
                .all(|call| !call.args.iter().any(|arg| arg == "prune"))
        );
        assert!(calls.iter().all(|call| {
            call.args.first().is_some_and(|arg| arg == "container")
                && !call
                    .args
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "image" | "builder" | "buildx" | "system"))
        }));
    }

    #[test]
    fn live_stream_bytes_exactly_match_step_report_and_json_is_valid() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let executor = FakeExecutor::new(
            &clock,
            vec![
                output(0, "", ""),
                output(0, "container-id-7", ""),
                output(0, "", ""),
                output(0, "build-out\n", "build-warning\n"),
                output(0, "lint-out\n", ""),
                output(0, "test-out\n", "test-warning\n"),
                output(0, "", ""),
            ],
        );
        let sink = RecordingSink::default();
        let report = DockerRunner::new_with_sink(&executor, clock, sink.clone())
            .run(&spec(true))
            .unwrap();
        let expected_stdout: String = report
            .steps
            .iter()
            .map(|step| step.stdout.as_str())
            .collect();
        let expected_stderr: String = report
            .steps
            .iter()
            .map(|step| step.stderr.as_str())
            .collect();
        assert_eq!(executor.invocations().len(), 7);
        assert_eq!(expected_stdout, "build-out\nlint-out\ntest-out\n");
        assert_eq!(expected_stderr, "build-warning\ntest-warning\n");
        assert_eq!(&*sink.stdout.lock().unwrap(), expected_stdout.as_bytes());
        assert_eq!(&*sink.stderr.lock().unwrap(), expected_stderr.as_bytes());
        assert_eq!(
            report
                .steps
                .iter()
                .flat_map(|step| &step.stdout_bytes)
                .copied()
                .collect::<Vec<_>>(),
            *sink.stdout.lock().unwrap()
        );
        assert_eq!(
            report
                .steps
                .iter()
                .flat_map(|step| &step.stderr_bytes)
                .copied()
                .collect::<Vec<_>>(),
            *sink.stderr.lock().unwrap()
        );
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("run.json");
        write_report_json(&report, &path).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(json["status"]["status"], "succeeded");
        assert_eq!(json["source_snapshot"]["file_count"], 7);
        assert!(
            json["image_digest"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
    }

    #[test]
    fn report_publish_is_no_overwrite_atomic_and_concurrency_safe() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let executor = FakeExecutor::new(&clock, success_outputs(3));
        let report = DockerRunner::new(&executor, clock)
            .run(&spec(true))
            .unwrap();
        let temp = tempfile::tempdir().unwrap();

        let existing = temp.path().join("existing.json");
        fs::write(&existing, b"{\"preserved\":true}").unwrap();
        assert!(write_report_json(&report, &existing).is_err());
        let preserved: serde_json::Value =
            serde_json::from_slice(&fs::read(&existing).unwrap()).unwrap();
        assert_eq!(preserved["preserved"], true);

        let shared = temp.path().join("shared.json");
        let successes = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| write_report_json(&report, &shared).is_ok()))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .filter(|success| *success)
                .count()
        });
        assert_eq!(successes, 1);
        serde_json::from_slice::<serde_json::Value>(&fs::read(&shared).unwrap()).unwrap();

        std::thread::scope(|scope| {
            let report = &report;
            let handles: Vec<_> = (0..8)
                .map(|index| {
                    let path = temp.path().join(format!("distinct-{index}.json"));
                    scope.spawn(move || write_report_json(report, &path))
                })
                .collect();
            for handle in handles {
                handle.join().unwrap().unwrap();
            }
        });
        let leftovers: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary reports leaked: {leftovers:?}"
        );
    }

    struct ByteAtATime {
        bytes: Vec<u8>,
        offset: usize,
    }

    impl Read for ByteAtATime {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.offset == self.bytes.len() {
                return Ok(0);
            }
            buffer[0] = self.bytes[self.offset];
            self.offset += 1;
            Ok(1)
        }
    }

    #[test]
    fn stream_capture_is_lossless_for_split_utf8_and_invalid_bytes() {
        let original = vec![b'a', 0xf0, 0x9f, 0x98, 0x80, 0xff, b'z'];
        let mut live = Vec::new();
        let captured = read_stream(
            ByteAtATime {
                bytes: original.clone(),
                offset: 0,
            },
            |chunk| live.extend_from_slice(chunk),
        )
        .unwrap();
        assert_eq!(live, original);
        assert_eq!(captured, original);
        assert_ne!(String::from_utf8_lossy(&captured).as_bytes(), captured);
    }

    #[test]
    fn report_lossless_bytes_exactly_reconstruct_non_utf8_live_output() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let raw = vec![0xf0, 0x9f, 0x98, 0x80, 0xff, b'\n'];
        let executor = RawStepExecutor {
            clock: Rc::clone(&clock.0),
            control_calls: Cell::new(0),
            bytes: raw.clone(),
        };
        let sink = RecordingSink::default();
        let report = DockerRunner::new_with_sink(&executor, clock, sink.clone())
            .run(&spec(true))
            .unwrap();
        let from_report: Vec<u8> = report
            .steps
            .iter()
            .flat_map(|step| step.stdout_bytes.iter().copied())
            .collect();
        assert_eq!(from_report, raw.repeat(report.steps.len()));
        assert_eq!(from_report, *sink.stdout.lock().unwrap());
        assert!(report.steps[0].stdout.contains('\u{fffd}'));
        let json = serde_json::to_vec(&report).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        let recovered: Vec<u8> = value["steps"][0]["stdout_bytes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|byte| byte.as_u64().unwrap() as u8)
            .collect();
        assert_eq!(recovered, raw);
    }

    #[test]
    fn failure_report_is_valid_json_and_keep_on_failure_skips_cleanup() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let executor = FakeExecutor::new(
            &clock,
            vec![
                output(0, "", ""),
                output(0, "container-id-7", ""),
                output(0, "", ""),
                output(2, "partial", "failed"),
            ],
        );
        let mut request = spec(true);
        request.keep_on_failure = true;
        let report = DockerRunner::new(&executor, clock).run(&request).unwrap();
        assert_eq!(report.cleanup, CleanupResult::RetainedOnFailure);
        assert_eq!(executor.invocations().len(), 4);
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("failed.json");
        write_report_json(&report, &path).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(json["status"]["status"], "command_failed");
        assert_eq!(json["cleanup"], "retained_on_failure");
    }

    #[test]
    fn command_failure_obeys_fail_fast() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let executor = FakeExecutor::new(
            &clock,
            vec![
                output(0, "", ""),
                output(0, "container-id-7", ""),
                output(0, "", ""),
                output(23, "", "compile failed"),
                output(0, "", ""),
            ],
        );
        let report = DockerRunner::new(&executor, clock)
            .run(&spec(true))
            .unwrap();
        assert_eq!(
            report.status,
            RunStatus::CommandFailed {
                phase: StepPhase::Build,
                step: "compile".to_owned(),
                exit_code: Some(23),
            }
        );
        assert_eq!(report.steps.len(), 1);
        assert_eq!(executor.invocations().len(), 5);
    }

    #[test]
    fn disabled_fail_fast_runs_remaining_build_and_test_steps() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let executor = FakeExecutor::new(
            &clock,
            vec![
                output(0, "", ""),
                output(0, "container-id-7", ""),
                output(0, "", ""),
                output(9, "", "compile failed"),
                output(0, "", ""),
                output(0, "", ""),
                output(0, "", ""),
            ],
        );
        let report = DockerRunner::new(&executor, clock)
            .run(&spec(false))
            .unwrap();
        assert!(matches!(report.status, RunStatus::CommandFailed { .. }));
        assert_eq!(report.steps.len(), 3);
        assert_eq!(report.steps[2].phase, StepPhase::Test);
    }

    #[test]
    fn timeout_has_a_deterministic_status_and_still_cleans_up() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let executor = FakeExecutor::new(
            &clock,
            vec![
                output(0, "", ""),
                output(0, "container-id-7", ""),
                output(0, "", ""),
                timed_out(),
                output(0, "", ""),
            ],
        );
        let report = DockerRunner::new(&executor, clock)
            .run(&spec(true))
            .unwrap();
        assert_eq!(report.status, RunStatus::TimedOut);
        assert_eq!(report.steps[0].status, StepStatus::TimedOut);
        assert_eq!(executor.invocations().last().unwrap().args[1], "rm");
    }

    #[test]
    fn interrupted_create_reconciles_and_removes_only_the_exact_owned_container() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let specification = spec(true);
        let inspect = format!(
            "container-id-7|{}|{}",
            specification.task_id, specification.repository_id
        );
        let executor = FakeExecutor::new(
            &clock,
            vec![
                output(0, "", ""),
                timed_out(),
                output(0, &inspect, ""),
                output(0, "", ""),
            ],
        );
        let registered = Rc::new(RefCell::new(None));
        let observed = Rc::clone(&registered);
        let report = DockerRunner::new(&executor, clock)
            .run_with_container_hook(&specification, move |id| {
                *observed.borrow_mut() = Some(id.to_owned());
                Ok(())
            })
            .unwrap();
        assert!(matches!(report.status, RunStatus::TimedOut));
        assert_eq!(report.container_id.as_deref(), Some("container-id-7"));
        assert_eq!(report.cleanup, CleanupResult::Removed);
        assert_eq!(registered.borrow().as_deref(), Some("container-id-7"));
        let calls = executor.invocations();
        assert_eq!(calls[2].args[1], "inspect");
        assert_eq!(calls[3].args[1], "rm");
    }

    #[test]
    fn interrupted_create_never_removes_a_name_with_mismatched_labels() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let executor = FakeExecutor::new(
            &clock,
            vec![
                output(0, "", ""),
                timed_out(),
                output(0, "foreign|other-task|sha256:foreign", ""),
            ],
        );
        let report = DockerRunner::new(&executor, clock)
            .run(&spec(true))
            .unwrap();
        assert_eq!(report.cleanup, CleanupResult::Failed);
        assert!(
            report
                .cleanup_error
                .as_deref()
                .unwrap()
                .contains("ownership labels")
        );
        assert_eq!(executor.invocations().len(), 3);
    }

    #[test]
    fn interrupted_create_retries_an_initial_absence_until_owned_container_appears() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let specification = spec(true);
        let inspect = format!(
            "late-id|{}|{}",
            specification.task_id, specification.repository_id
        );
        let executor = FakeExecutor::new(
            &clock,
            vec![
                output(0, "", ""),
                timed_out(),
                output(1, "", "Error: No such container: repo-sandbox-task-7"),
                output(0, &inspect, ""),
                output(0, "", ""),
            ],
        );
        let report = DockerRunner::new(&executor, clock)
            .run(&specification)
            .unwrap();
        assert_eq!(report.container_id.as_deref(), Some("late-id"));
        assert_eq!(report.cleanup, CleanupResult::Removed);
        assert_eq!(executor.invocations().len(), 5);
    }

    #[test]
    fn interrupted_create_does_not_treat_unrelated_not_found_as_container_absence() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let executor = FakeExecutor::new(
            &clock,
            vec![
                output(0, "", ""),
                timed_out(),
                output(1, "", "credential helper not found"),
            ],
        );
        let report = DockerRunner::new(&executor, clock)
            .run(&spec(true))
            .unwrap();
        assert_eq!(report.cleanup, CleanupResult::Failed);
        assert!(
            report
                .cleanup_error
                .as_deref()
                .unwrap()
                .contains("credential helper")
        );
        assert_eq!(executor.invocations().len(), 3);
    }

    #[test]
    fn memory_and_temporary_storage_exhaustion_are_distinct() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let memory = FakeExecutor::new(
            &clock,
            vec![
                output(0, "", ""),
                output(0, "container-id-7", ""),
                output(0, "", ""),
                output(137, "", "Killed"),
                output(0, "true\n", ""),
                output(0, "", ""),
            ],
        );
        let report = DockerRunner::new(&memory, clock.clone())
            .run(&spec(true))
            .unwrap();
        assert!(matches!(
            report.status,
            RunStatus::ResourceExceeded {
                limit: ResourceLimit::Memory,
                ..
            }
        ));

        clock.0.set(0);
        let storage = FakeExecutor::new(
            &clock,
            vec![
                output(0, "", ""),
                output(0, "container-id-7", ""),
                output(0, "", ""),
                output(1, "", "write: No space left on device"),
                output(0, "", ""),
            ],
        );
        let report = DockerRunner::new(&storage, clock).run(&spec(true)).unwrap();
        assert!(matches!(
            report.status,
            RunStatus::ResourceExceeded {
                limit: ResourceLimit::TemporaryStorage,
                ..
            }
        ));
    }

    #[test]
    fn preexisting_label_is_never_started_or_removed() {
        let clock = FakeClock(Rc::new(Cell::new(0)));
        let executor = FakeExecutor::new(&clock, vec![output(0, "shared-id\n", "")]);
        let report = DockerRunner::new(&executor, clock)
            .run(&spec(true))
            .unwrap();
        assert!(matches!(
            report.status,
            RunStatus::InfrastructureFailed { .. }
        ));
        assert!(report.container_id.is_none());
        assert_eq!(executor.invocations().len(), 1);
    }

    #[test]
    fn cleanup_uses_the_bounded_post_cancellation_execution_path() {
        let executor = CleanupRoutingExecutor {
            ordinary_calls: Cell::new(0),
            cleanup_calls: Cell::new(0),
        };
        DockerRunner::new(&executor, FakeClock(Rc::new(Cell::new(0))))
            .cleanup("owned-container")
            .unwrap();
        assert_eq!(executor.cleanup_calls.get(), 1);
        assert_eq!(executor.ordinary_calls.get(), 0);
    }

    #[test]
    fn invalid_specs_fail_before_docker() {
        let mut invalid = spec(true);
        invalid.task_id = "BAD/task".to_owned();
        assert!(plan(&invalid).is_err());
        invalid = spec(true);
        invalid.resources.temporary_storage_mb = 0;
        assert!(plan(&invalid).is_err());
    }

    /// Optional end-to-end check. The image is caller-visible cache state; the
    /// runner owns and removes only the uniquely labelled container it creates.
    #[test]
    #[ignore = "requires an accessible Linux Docker daemon and busybox:1.36"]
    fn docker_one_shot_job_smoke() {
        let mut request = spec(true);
        let artifacts = tempfile::tempdir().unwrap();
        request.task_id = format!("issue13-smoke-{}", std::process::id());
        request.image = ImageRef::new("busybox:1.36").unwrap();
        request.build = vec![RunStep {
            name: "build".to_owned(),
            command: "mkdir -p target && echo built > target/result".to_owned(),
        }];
        request.test = vec![RunStep {
            name: "test".to_owned(),
            command: "test \"$(cat target/result)\" = built".to_owned(),
        }];
        request.resources = RunResources {
            cpu_count: 1,
            memory_mb: 128,
            temporary_storage_mb: 32,
        };
        request.timeout_ms = 30_000;
        request.config_summary.artifact_directories = vec![PathBuf::from("target")];
        request.artifact_export_root = Some(artifacts.path().to_owned());
        let report = DockerRunner::new(SystemDockerExecutor, SystemClock::default())
            .run(&request)
            .unwrap();
        assert_eq!(report.status, RunStatus::Succeeded, "{report:?}");
        assert_eq!(report.steps.len(), 2);
        assert_eq!(
            fs::read_to_string(artifacts.path().join("target/result")).unwrap(),
            "built\n"
        );
        assert_eq!(
            report.exported_artifacts,
            vec![artifacts.path().canonicalize().unwrap().join("target")]
        );
        assert!(report.artifact_error.is_none());
        assert!(report.cleanup_error.is_none());
    }

    /// Real failure injection for stage attribution, timeouts and retained diagnostics.
    /// Every container has a per-process ownership label; the one deliberately retained
    /// container is verified by label before this test removes that exact ID.
    #[test]
    #[ignore = "requires an accessible Linux Docker daemon and busybox:1.36"]
    fn docker_build_test_timeout_and_keep_on_failure_matrix() {
        let unique = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let base = || {
            let mut request = spec(true);
            request.image = ImageRef::new("busybox:1.36").unwrap();
            request.resources = RunResources {
                cpu_count: 1,
                memory_mb: 128,
                temporary_storage_mb: 32,
            };
            request.config_summary.artifact_directories.clear();
            request.artifact_export_root = None;
            request
        };

        let mut build_failure = base();
        build_failure.task_id = format!("e2e-build-{unique}");
        build_failure.build = vec![RunStep {
            name: "compile-dogfood".to_owned(),
            command: "echo stage=build; exit 41".to_owned(),
        }];
        build_failure.test = vec![RunStep {
            name: "must-not-run".to_owned(),
            command: "exit 99".to_owned(),
        }];
        let build_report = DockerRunner::new(SystemDockerExecutor, SystemClock::default())
            .run(&build_failure)
            .unwrap();
        assert_eq!(
            build_report.status,
            RunStatus::CommandFailed {
                phase: StepPhase::Build,
                step: "compile-dogfood".to_owned(),
                exit_code: Some(41),
            }
        );
        assert_eq!(build_report.steps.len(), 1);
        assert_eq!(build_report.cleanup, CleanupResult::Removed);

        let mut test_failure = base();
        test_failure.task_id = format!("e2e-test-{unique}");
        test_failure.build = vec![RunStep {
            name: "compile-dogfood".to_owned(),
            command: "echo stage=build".to_owned(),
        }];
        test_failure.test = vec![RunStep {
            name: "unit-dogfood".to_owned(),
            command: "echo stage=test >&2; exit 42".to_owned(),
        }];
        let test_report = DockerRunner::new(SystemDockerExecutor, SystemClock::default())
            .run(&test_failure)
            .unwrap();
        assert_eq!(
            test_report.status,
            RunStatus::CommandFailed {
                phase: StepPhase::Test,
                step: "unit-dogfood".to_owned(),
                exit_code: Some(42),
            }
        );
        assert_eq!(test_report.cleanup, CleanupResult::Removed);

        let mut timeout = base();
        timeout.task_id = format!("e2e-timeout-{unique}");
        timeout.timeout_ms = 2_000;
        timeout.build = vec![RunStep {
            name: "bounded-build".to_owned(),
            command: "echo stage=build-timeout; sleep 30".to_owned(),
        }];
        timeout.test = vec![RunStep {
            name: "must-not-run".to_owned(),
            command: "exit 99".to_owned(),
        }];
        let timeout_report = DockerRunner::new(SystemDockerExecutor, SystemClock::default())
            .run(&timeout)
            .unwrap();
        assert_eq!(timeout_report.status, RunStatus::TimedOut);
        assert_eq!(timeout_report.cleanup, CleanupResult::Removed);

        let mut retained = base();
        retained.task_id = format!("e2e-keep-{unique}");
        retained.keep_on_failure = true;
        retained.build = vec![RunStep {
            name: "retained-build".to_owned(),
            command: "echo stage=build-retained; exit 43".to_owned(),
        }];
        retained.test = vec![RunStep {
            name: "must-not-run".to_owned(),
            command: "exit 99".to_owned(),
        }];
        let retained_report = DockerRunner::new(SystemDockerExecutor, SystemClock::default())
            .run(&retained)
            .unwrap();
        assert_eq!(retained_report.cleanup, CleanupResult::RetainedOnFailure);
        let container_id = retained_report.container_id.clone().unwrap();
        let inspect = ProcessInvocation {
            program: "docker".to_owned(),
            args: vec![
                "container".to_owned(),
                "inspect".to_owned(),
                "--format".to_owned(),
                format!("{{{{ index .Config.Labels \"{TASK_LABEL}\" }}}}"),
                container_id.clone(),
            ],
            current_dir: None,
        };
        let inspected = SystemDockerExecutor
            .execute(&inspect, CLEANUP_TIMEOUT)
            .unwrap();
        assert_eq!(inspected.exit_code, Some(0));
        assert_eq!(inspected.stdout.trim(), retained.task_id);
        let remove = docker(vec!["container", "rm", "--force", &container_id]);
        let removed = SystemDockerExecutor
            .execute(&remove, CLEANUP_TIMEOUT)
            .unwrap();
        assert_eq!(removed.exit_code, Some(0), "{}", removed.stderr);

        let serialized =
            serde_json::to_string(&[build_report, test_report, timeout_report, retained_report])
                .unwrap();
        assert!(!serialized.contains("issue16-private-credential-marker"));
        println!("stage=build failure_exit=41");
        println!("stage=test failure_exit=42");
        println!("stage=timeout status=timed_out");
        println!("stage=cleanup status=retained_then_owned_removed");
        println!("credential_scan=passed");
    }
}
