//! Local Docker adapter for bounded, one-shot build and test jobs.

use crate::buildkit::{ProcessInvocation, ProcessOutput};
use repo_sandbox_core::runner::{
    ResourceLimit, RunReport, RunSpec, RunStatus, StepPhase, StepResult, StepStatus,
};
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const TASK_LABEL: &str = "io.repo-sandbox.task-id";
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

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
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemDockerExecutor;

impl DockerExecutor for SystemDockerExecutor {
    fn execute(
        &self,
        invocation: &ProcessInvocation,
        timeout: Duration,
    ) -> io::Result<ProcessOutput> {
        let mut child = Command::new(&invocation.program)
            .args(&invocation.args)
            .current_dir(invocation.current_dir.as_deref().unwrap_or(Path::new(".")))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdout_reader = thread::spawn(move || read_stream(stdout));
        let stderr_reader = thread::spawn(move || read_stream(stderr));
        let deadline = Instant::now() + timeout;
        let (status, interrupted) = loop {
            if Instant::now() >= deadline {
                child.kill()?;
                break (child.wait()?, true);
            }
            if let Some(status) = child.try_wait()? {
                break (status, false);
            }
            thread::sleep(Duration::from_millis(20));
        };
        Ok(ProcessOutput {
            exit_code: status.code(),
            stdout: join_reader(stdout_reader)?,
            stderr: join_reader(stderr_reader)?,
            interrupted,
        })
    }
}

fn read_stream(mut stream: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_reader(reader: thread::JoinHandle<io::Result<Vec<u8>>>) -> io::Result<String> {
    let bytes = reader
        .join()
        .map_err(|_| io::Error::other("process output reader panicked"))??;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
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
    let temporary = format!(
        "/tmp:rw,nosuid,nodev,size={}m",
        spec.resources.temporary_storage_mb
    );
    let writable_layer = format!("size={}m", spec.resources.temporary_storage_mb);
    let create = docker(vec![
        "container",
        "create",
        "--name",
        &name,
        "--label",
        &label,
        "--network",
        "bridge",
        "--cpus",
        &spec.resources.cpu_count.to_string(),
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
        spec.image.as_str(),
        "/bin/sh",
        "-c",
        "trap 'exit 0' TERM INT; while :; do sleep 3600; done",
    ]);
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
                "-lc",
                &step.command,
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
    if spec.build.is_empty() || spec.test.is_empty() {
        return Err(PlanError(
            "build and test must each contain at least one step".to_owned(),
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
    Ok(())
}

pub struct DockerRunner<E, C> {
    executor: E,
    clock: C,
}

impl<E, C> DockerRunner<E, C> {
    pub const fn new(executor: E, clock: C) -> Self {
        Self { executor, clock }
    }
}

impl<E: DockerExecutor, C: Clock> DockerRunner<E, C> {
    pub fn run(&self, spec: &RunSpec) -> Result<RunReport, PlanError> {
        let run_plan = plan(spec)?;
        let started = self.clock.now();
        let mut report = RunReport {
            task_id: spec.task_id.clone(),
            container_id: None,
            started_at_unix_ms: started.unix_ms,
            ended_at_unix_ms: started.unix_ms,
            duration_ms: 0,
            status: RunStatus::Succeeded,
            steps: Vec::new(),
            cleanup_error: None,
        };

        if let Err(status) = self.ensure_unowned(&run_plan, spec, started.monotonic_ms) {
            report.status = status;
            return Ok(self.finish(report, started));
        }
        let create = match self.execute_remaining(&run_plan.create, spec, started.monotonic_ms) {
            Ok(output) if output.interrupted => {
                report.status = RunStatus::TimedOut;
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

        match self.execute_remaining(&run_plan.start, spec, started.monotonic_ms) {
            Ok(output) if output.interrupted => report.status = RunStatus::TimedOut,
            Ok(output) if output.exit_code != Some(0) => {
                report.status = infrastructure("start owned container", output.stderr)
            }
            Err(error) => {
                report.status = infrastructure("start owned container", error.to_string())
            }
            Ok(_) => self.run_steps(spec, &run_plan, started, &container_id, &mut report),
        }

        report.cleanup_error = self
            .cleanup(&container_id)
            .err()
            .map(|error| error.to_string());
        Ok(self.finish(report, started))
    }

    fn ensure_unowned(
        &self,
        run_plan: &DockerRunPlan,
        spec: &RunSpec,
        started_ms: u64,
    ) -> Result<(), RunStatus> {
        match self.execute_remaining(&run_plan.ownership_check, spec, started_ms) {
            Ok(output) if output.interrupted => Err(RunStatus::TimedOut),
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
            let output = self.execute_remaining(&step.invocation, spec, started.monotonic_ms);
            let step_ended = self.clock.now();
            let (status, exit_code, stdout, stderr) = match output {
                Err(error) => (
                    StepStatus::InfrastructureFailed,
                    None,
                    String::new(),
                    error.to_string(),
                ),
                Ok(output) if output.interrupted => (
                    StepStatus::TimedOut,
                    output.exit_code,
                    output.stdout,
                    output.stderr,
                ),
                Ok(output) if output.exit_code == Some(0) => (
                    StepStatus::Succeeded,
                    output.exit_code,
                    output.stdout,
                    output.stderr,
                ),
                Ok(output) => {
                    let limit = self.resource_limit(container_id, &output);
                    let status = limit.map_or(StepStatus::CommandFailed, |limit| {
                        StepStatus::ResourceExceeded { limit }
                    });
                    (status, output.exit_code, output.stdout, output.stderr)
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

    fn cleanup(&self, container_id: &str) -> io::Result<()> {
        let invocation = docker(vec!["container", "rm", "--force", container_id]);
        let output = self.executor.execute(&invocation, CLEANUP_TIMEOUT)?;
        if output.exit_code == Some(0) {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "remove owned container `{container_id}`: {}",
                output.stderr.trim()
            )))
        }
    }

    fn finish(&self, mut report: RunReport, started: ClockReading) -> RunReport {
        let ended = self.clock.now();
        report.ended_at_unix_ms = ended.unix_ms;
        report.duration_ms = ended.monotonic_ms.saturating_sub(started.monotonic_ms);
        report
    }
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
    use repo_sandbox_core::build::ImageRef;
    use repo_sandbox_core::config::Platform;
    use repo_sandbox_core::runner::{RunResources, RunStep};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

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
            image: ImageRef::new("repo-sandbox/task@sha256:abc").unwrap(),
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
            ["--network", "bridge"],
            ["--cpus", "2"],
            ["--memory", "2048m"],
            ["--memory-swap", "2048m"],
            ["--security-opt", "no-new-privileges=true"],
            ["--cap-drop", "ALL"],
        ] {
            assert!(args.windows(2).any(|actual| actual == pair));
        }
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
        request.task_id = format!("issue12-smoke-{}", std::process::id());
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
        let report = DockerRunner::new(SystemDockerExecutor, SystemClock::default())
            .run(&request)
            .unwrap();
        assert_eq!(report.status, RunStatus::Succeeded, "{report:?}");
        assert_eq!(report.steps.len(), 2);
        assert!(report.cleanup_error.is_none());
    }
}
