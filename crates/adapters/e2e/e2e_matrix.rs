use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Matrix {
    version: u32,
    scenarios: Vec<Scenario>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    id: String,
    description: String,
    tier: Tier,
    targets: Vec<String>,
    covers: Vec<String>,
    #[serde(default)]
    required_env: Vec<String>,
    #[serde(default)]
    redact_env: Vec<String>,
    #[serde(default)]
    redact_file_env: Vec<String>,
    timeout_seconds: u64,
    command: CommandSpec,
    expected: Expected,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Tier {
    Required,
    OptIn,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandSpec {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    exit_code: i32,
    #[serde(default)]
    log_contains: Vec<String>,
    #[serde(default)]
    log_not_contains: Vec<String>,
    #[serde(default)]
    artifacts: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ScenarioReport {
    id: String,
    target: String,
    status: &'static str,
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u128,
    log: PathBuf,
    assertions: Vec<String>,
}

#[derive(Debug)]
struct Args {
    matrix: PathBuf,
    target: String,
    output: Option<PathBuf>,
    scenario: Option<String>,
    list: bool,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("e2e matrix failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let root = repository_root()?;
    let args = parse_args(&root)?;
    let source = fs::read_to_string(&args.matrix)
        .map_err(|error| format!("read {}: {error}", args.matrix.display()))?;
    let matrix: Matrix = serde_yaml::from_str(&source)
        .map_err(|error| format!("parse {}: {error}", args.matrix.display()))?;
    validate(&matrix)?;
    if args.list {
        for scenario in &matrix.scenarios {
            println!(
                "{}\t{:?}\t{}\t{}",
                scenario.id,
                scenario.tier,
                scenario.targets.join(","),
                scenario.description
            );
        }
        return Ok(());
    }

    let run_id = run_id();
    let output = args
        .output
        .unwrap_or_else(|| root.join("target/e2e").join(&run_id));
    fs::create_dir_all(&output)
        .map_err(|error| format!("create output {}: {error}", output.display()))?;
    let mut selected = 0_usize;
    let mut failed = 0_usize;
    let mut skipped = 0_usize;
    for scenario in matrix.scenarios.iter().filter(|scenario| {
        scenario.targets.iter().any(|target| target == &args.target)
            && args
                .scenario
                .as_ref()
                .is_none_or(|selected| selected == &scenario.id)
    }) {
        selected += 1;
        let missing = scenario
            .required_env
            .iter()
            .filter(|name| env::var_os(name).is_none())
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            if scenario.tier == Tier::Required {
                return Err(format!(
                    "required scenario {} is missing environment: {}",
                    scenario.id,
                    missing.join(", ")
                ));
            }
            skipped += 1;
            println!(
                "SKIP [{}] {} (set {})",
                args.target,
                scenario.id,
                missing.join(", ")
            );
            continue;
        }
        println!(
            "RUN  [{}] {} — {}",
            args.target, scenario.id, scenario.description
        );
        let scenario_root = output.join(&scenario.id);
        fs::create_dir(&scenario_root).map_err(|error| {
            format!(
                "create unique scenario output {}: {error}",
                scenario_root.display()
            )
        })?;
        let report = execute(scenario, &args.target, &root, &scenario_root, &run_id)?;
        let report_path = scenario_root.join("report.json");
        let mut report_file = File::create(&report_path)
            .map_err(|error| format!("create {}: {error}", report_path.display()))?;
        serde_json::to_writer_pretty(&mut report_file, &report)
            .map_err(|error| format!("write {}: {error}", report_path.display()))?;
        report_file
            .write_all(b"\n")
            .map_err(|error| format!("finish {}: {error}", report_path.display()))?;
        if report.status == "passed" {
            println!(
                "PASS [{}] {} ({} ms)",
                args.target, scenario.id, report.duration_ms
            );
        } else {
            failed += 1;
            eprintln!(
                "FAIL [{}] {}: {}",
                args.target,
                scenario.id,
                report.assertions.join("; ")
            );
        }
    }
    if selected == 0 {
        return Err(format!("no scenarios selected for target {}", args.target));
    }
    println!(
        "matrix target={} selected={} passed={} skipped={} failed={} output={}",
        args.target,
        selected,
        selected - skipped - failed,
        skipped,
        failed,
        output.display()
    );
    if failed == 0 {
        Ok(())
    } else {
        Err(format!("{failed} scenario(s) failed"))
    }
}

fn execute(
    scenario: &Scenario,
    target: &str,
    root: &Path,
    result_dir: &Path,
    run_id: &str,
) -> Result<ScenarioReport, String> {
    let expand = |value: &str| -> Result<String, String> {
        let mut value = value
            .replace("${ROOT}", &command_path(root))
            .replace("${RESULT_DIR}", &command_path(result_dir))
            .replace("${RUN_ID}", run_id)
            .replace("${SCENARIO_ID}", &scenario.id);
        while let Some(start) = value.find("${ENV:") {
            let tail = &value[start + 6..];
            let end = tail.find('}').ok_or_else(|| {
                format!(
                    "scenario {} has an unterminated environment expansion",
                    scenario.id
                )
            })?;
            let name = &tail[..end];
            let replacement = env::var(name)
                .map_err(|_| format!("scenario {} requires environment {name}", scenario.id))?;
            value.replace_range(start..start + 6 + end + 1, &replacement);
        }
        Ok(value)
    };
    let program = resolve_program(&expand(&scenario.command.program)?)?;
    let command_args = scenario
        .command
        .args
        .iter()
        .map(|value| expand(value))
        .collect::<Result<Vec<_>, _>>()?;
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("setsid");
        command.arg(&program).args(&command_args);
        command
    };
    #[cfg(not(target_os = "linux"))]
    let mut command = {
        let mut command = Command::new(&program);
        command.args(&command_args);
        command
    };
    command
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("REPO_SANDBOX_E2E_RUN_ID", run_id)
        .env("REPO_SANDBOX_E2E_RESULT_DIR", result_dir);
    for (name, value) in &scenario.command.env {
        command.env(name, expand(value)?);
    }
    #[cfg(target_os = "linux")]
    probe_pidfd_cleanup()
        .map_err(|error| format!("Linux matrix requires pidfd-safe cleanup: {error}"))?;
    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|error| format!("start scenario {}: {error}", scenario.id))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "capture stdout".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "capture stderr".to_owned())?;
    let stdout_reader = thread::spawn(move || read_all(stdout));
    let stderr_reader = thread::spawn(move || read_all(stderr));
    let deadline = started + Duration::from_secs(scenario.timeout_seconds);
    let (status, timed_out) = loop {
        if let Some(status) = poll_scenario(&mut child)
            .map_err(|error| format!("wait for scenario {}: {error}", scenario.id))?
        {
            break (status, false);
        }
        if Instant::now() >= deadline {
            kill_process_tree(&mut child).map_err(|error| {
                format!("kill timed out scenario tree {}: {error}", scenario.id)
            })?;
            let status = child
                .wait()
                .map_err(|error| format!("reap timed out scenario {}: {error}", scenario.id))?;
            break (status, true);
        }
        thread::sleep(Duration::from_millis(100));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "stdout reader panicked".to_owned())?
        .map_err(|error| format!("read scenario stdout: {error}"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "stderr reader panicked".to_owned())?
        .map_err(|error| format!("read scenario stderr: {error}"))?;
    let raw_text = format!(
        "{}{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    let mut secrets = scenario
        .redact_env
        .iter()
        .filter_map(|name| env::var(name).ok())
        .collect::<Vec<_>>();
    secrets.extend(scenario.redact_file_env.iter().flat_map(|name| {
        let Some(contents) = env::var_os(name).and_then(|path| fs::read_to_string(path).ok())
        else {
            return Vec::new();
        };
        file_secret_variants(contents)
    }));
    let stdout = redact(&stdout, &secrets);
    let stderr = redact(&stderr, &secrets);
    let mut log = Vec::with_capacity(stdout.len() + stderr.len() + 64);
    log.extend_from_slice(b"=== stdout ===\n");
    log.extend_from_slice(&stdout);
    log.extend_from_slice(b"\n=== stderr ===\n");
    log.extend_from_slice(&stderr);
    let log_path = result_dir.join("scenario.log");
    fs::write(&log_path, &log).map_err(|error| format!("write {}: {error}", log_path.display()))?;
    io::stdout()
        .write_all(&stdout)
        .map_err(|error| error.to_string())?;
    io::stderr()
        .write_all(&stderr)
        .map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&log);
    let mut assertions = Vec::new();
    if timed_out {
        assertions.push(format!(
            "matrix timeout after {} seconds",
            scenario.timeout_seconds
        ));
    }
    if status.code() != Some(scenario.expected.exit_code) {
        assertions.push(format!(
            "exit code expected {}, got {:?}",
            scenario.expected.exit_code,
            status.code()
        ));
    }
    for needle in &scenario.expected.log_contains {
        let needle = expand(needle)?;
        if !text.contains(&needle) {
            assertions.push(format!("log missing {needle:?}"));
        }
    }
    for needle in &scenario.expected.log_not_contains {
        let needle = expand(needle)?;
        if raw_text.contains(&needle) {
            assertions.push("log unexpectedly contains a forbidden marker".to_owned());
        }
    }
    for artifact in &scenario.expected.artifacts {
        let artifact = result_dir.join(artifact);
        if !artifact.exists() {
            assertions.push(format!("artifact missing: {}", artifact.display()));
        }
    }
    Ok(ScenarioReport {
        id: scenario.id.clone(),
        target: target.to_owned(),
        status: if assertions.is_empty() {
            "passed"
        } else {
            "failed"
        },
        exit_code: status.code(),
        timed_out,
        duration_ms: started.elapsed().as_millis(),
        log: log_path,
        assertions,
    })
}

fn read_all(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(windows)]
fn kill_process_tree(child: &mut std::process::Child) -> io::Result<()> {
    let status = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        child.kill()
    }
}

#[cfg(target_os = "linux")]
fn task_session_members(session: u32) -> io::Result<BTreeSet<u32>> {
    let own = fs::read_to_string("/proc/self/stat")?;
    if process_session(&own).is_some_and(|(_, own_session)| own_session == session) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refused to signal the matrix session",
        ));
    }
    let mut members = BTreeSet::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        // Processes may exit while /proc is enumerated. Only members of the
        // exact scenario session are eligible; user IDs and command names are
        // deliberately not ownership criteria.
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if let Some((_, found_session)) = process_session(&stat)
            && found_session == session
            && pid > 1
        {
            members.insert(pid);
        }
    }
    Ok(members)
}

#[cfg(target_os = "linux")]
fn process_session(stat: &str) -> Option<(u32, u32)> {
    // comm is parenthesized and may itself contain spaces or ')'. The fields
    // after its final ')' are state, ppid, pgrp, session, ... .
    let (_, fields) = stat.rsplit_once(')')?;
    let mut fields = fields.split_whitespace();
    fields.next()?;
    fields.next()?;
    Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
}

// These syscall numbers are shared by the supported Linux x86_64/aarch64 hosts.
#[cfg(target_os = "linux")]
fn open_pidfd(pid: u32) -> io::Result<File> {
    use std::os::fd::FromRawFd;
    unsafe extern "C" {
        fn syscall(number: std::ffi::c_long, ...) -> std::ffi::c_long;
    }
    let fd = unsafe { syscall(434, pid as i32, 0_u32) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

#[cfg(target_os = "linux")]
fn signal_pidfd(fd: &File, signal: i32) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn syscall(number: std::ffi::c_long, ...) -> std::ffi::c_long;
    }
    if unsafe {
        syscall(
            424,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<std::ffi::c_void>(),
            0_u32,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn probe_pidfd_cleanup() -> io::Result<()> {
    // Fail before spawning any scenario on kernels/sandboxes without these APIs.
    signal_pidfd(&open_pidfd(std::process::id())?, 0)
}

#[cfg(target_os = "linux")]
fn signal_task_session(session: u32, signal: i32) -> io::Result<usize> {
    let mut live = 0;
    for pid in task_session_members(session)? {
        let fd = match open_pidfd(pid) {
            Ok(fd) => fd,
            Err(error) if error.raw_os_error() == Some(3) => continue,
            Err(error) => return Err(error),
        };
        // Bind the process first, then recheck ownership. PID reuse between
        // enumeration and opening cannot cause us to signal an unrelated task.
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        if !process_session(&stat).is_some_and(|(_, found)| found == session)
            || process_state(&stat) == Some("Z")
        {
            continue;
        }
        live += 1;
        if let Err(error) = signal_pidfd(&fd, signal)
            && error.raw_os_error() != Some(3)
        {
            return Err(error);
        }
    }
    Ok(live)
}

#[cfg(target_os = "linux")]
fn kill_process_tree(child: &mut std::process::Child) -> io::Result<()> {
    // Keep the leader unreaped throughout cleanup so its unique SID cannot
    // be reused, even when TERM makes the leader exit before its descendants.
    signal_task_session(child.id(), 15)?;
    thread::sleep(Duration::from_millis(250));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // A live parent may fork between enumeration and its signal. Rescan
        // until no live member can create children or retain the log pipes.
        if signal_task_session(child.id(), 9)? == 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "scenario session did not exit after SIGKILL",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn process_state(stat: &str) -> Option<&str> {
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().next())
}

#[cfg(target_os = "linux")]
fn poll_scenario(child: &mut std::process::Child) -> io::Result<Option<std::process::ExitStatus>> {
    let stat = fs::read_to_string(format!("/proc/{}/stat", child.id()))?;
    if process_state(&stat) == Some("Z") {
        // A finished shell can leave new-PGID descendants holding log pipes.
        // Clean its session before reaping, then readers can reach EOF.
        kill_process_tree(child)?;
        return child.wait().map(Some);
    }
    Ok(None)
}

#[cfg(not(target_os = "linux"))]
fn poll_scenario(child: &mut std::process::Child) -> io::Result<Option<std::process::ExitStatus>> {
    child.try_wait()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn kill_process_tree(child: &mut std::process::Child) -> io::Result<()> {
    child.kill()
}

fn redact(bytes: &[u8], secrets: &[String]) -> Vec<u8> {
    let mut value = String::from_utf8_lossy(bytes).into_owned();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        value = value.replace(secret, "<redacted>");
    }
    value.into_bytes()
}

fn file_secret_variants(contents: String) -> Vec<String> {
    let trimmed = contents.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut variants = Vec::with_capacity(2);
    variants.push(contents.clone());
    if trimmed != contents {
        variants.push(trimmed.to_owned());
    }
    variants
}

fn command_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn resolve_program(program: &str) -> Result<String, String> {
    if program != "repo-bash" {
        return Ok(program.to_owned());
    }
    #[cfg(not(windows))]
    return Ok("bash".to_owned());
    #[cfg(windows)]
    {
        let program_files = env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        let candidate = program_files.join("Git/bin/bash.exe");
        if candidate.is_file() {
            Ok(candidate.to_string_lossy().into_owned())
        } else {
            Err("repo-bash requires Git for Windows bash.exe".to_owned())
        }
    }
}

fn validate(matrix: &Matrix) -> Result<(), String> {
    if matrix.version != 1 {
        return Err(format!("unsupported matrix version {}", matrix.version));
    }
    let mut ids = BTreeSet::new();
    for scenario in &matrix.scenarios {
        let valid_id = !scenario.id.is_empty()
            && scenario.id.len() <= 64
            && scenario.id.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            });
        if !valid_id {
            return Err(format!("invalid scenario id {:?}", scenario.id));
        }
        if !ids.insert(&scenario.id) {
            return Err(format!("duplicate scenario id {}", scenario.id));
        }
        if scenario.description.trim().is_empty()
            || scenario.targets.is_empty()
            || scenario.covers.is_empty()
            || scenario.timeout_seconds == 0
            || scenario.command.program.trim().is_empty()
        {
            return Err(format!(
                "scenario {} has incomplete fixed inputs",
                scenario.id
            ));
        }
        if scenario
            .redact_env
            .iter()
            .chain(&scenario.redact_file_env)
            .any(|name| !scenario.required_env.contains(name))
        {
            return Err(format!(
                "scenario {} redacts an environment value that is not required",
                scenario.id
            ));
        }
        for artifact in &scenario.expected.artifacts {
            let path = Path::new(artifact);
            if path.is_absolute()
                || path.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                return Err(format!("scenario {} has unsafe artifact path", scenario.id));
            }
        }
    }
    Ok(())
}

fn parse_args(root: &Path) -> Result<Args, String> {
    let mut args = env::args().skip(1);
    let mut parsed = Args {
        matrix: root.join("tests/e2e/scenarios.yaml"),
        target: "docker".to_owned(),
        output: None,
        scenario: None,
        list: false,
    };
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--matrix" => parsed.matrix = PathBuf::from(required_value(&mut args, "--matrix")?),
            "--target" => parsed.target = required_value(&mut args, "--target")?,
            "--output" => {
                parsed.output = Some(PathBuf::from(required_value(&mut args, "--output")?))
            }
            "--scenario" => parsed.scenario = Some(required_value(&mut args, "--scenario")?),
            "--list" => parsed.list = true,
            "--help" | "-h" => {
                println!(
                    "usage: e2e-matrix [--matrix PATH] [--target docker|wsl|vm] [--scenario ID] [--output PATH] [--list]"
                );
                std::process::exit(0);
            }
            _ => return Err(format!("unknown argument {argument}")),
        }
    }
    Ok(parsed)
}

fn required_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn repository_root() -> Result<PathBuf, String> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_owned)
        .ok_or_else(|| "locate repository root".to_owned())
}

fn run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::{file_secret_variants, redact};

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_session_worker() {
        use std::os::unix::process::CommandExt;
        let Ok(role) = std::env::var("REPO_SANDBOX_E2E_TEST_WORKER") else {
            return;
        };
        if role == "parent" || role == "parent-exit" {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", "tests::linux_session_worker", "--nocapture"])
                .env("REPO_SANDBOX_E2E_TEST_WORKER", "grandchild")
                .process_group(0);
            #[allow(
                clippy::zombie_processes,
                reason = "The parent-exit fixture deliberately leaves its child alive; the outer test cleans the owned session to reproduce and verify early-leader-exit cleanup"
            )]
            let mut child = child.spawn().unwrap();
            if role == "parent" {
                child.wait().unwrap();
            }
        } else {
            unsafe extern "C" {
                fn signal(signal: i32, handler: usize) -> usize;
            }
            // Confined to this explicitly spawned worker process.
            unsafe {
                signal(15, 1);
            } // SIGTERM, SIG_IGN
            std::fs::write(
                std::env::var_os("REPO_SANDBOX_E2E_TEST_PID").unwrap(),
                std::process::id().to_string(),
            )
            .unwrap();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timeout_kills_new_process_group_grandchild_after_session_leader_exits() {
        assert_session_cleanup(false);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completed_leader_cleans_new_process_group_and_releases_log_pipe() {
        assert_session_cleanup(true);
    }

    #[cfg(target_os = "linux")]
    fn assert_session_cleanup(leader_exits: bool) {
        use std::{
            fs,
            process::{Command, Stdio},
            thread,
            time::{Duration, Instant},
        };
        struct OwnedSession(std::process::Child, bool);
        impl Drop for OwnedSession {
            fn drop(&mut self) {
                if self.1 {
                    return;
                }
                let _ = super::kill_process_tree(&mut self.0);
                let _ = self.0.wait();
            }
        }
        super::probe_pidfd_cleanup().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let spawn = |role: &str, pid: &std::path::Path| {
            OwnedSession(
                Command::new("setsid")
                    .arg(std::env::current_exe().unwrap())
                    .args(["--exact", "tests::linux_session_worker", "--nocapture"])
                    .env("REPO_SANDBOX_E2E_TEST_WORKER", role)
                    .env("REPO_SANDBOX_E2E_TEST_PID", pid)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
                false,
            )
        };
        let owned_pid = directory.path().join("owned");
        let foreign_pid = directory.path().join("foreign");
        let mut owned = spawn(
            if leader_exits {
                "parent-exit"
            } else {
                "parent"
            },
            &owned_pid,
        );
        let mut stdout = owned.0.stdout.take().unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let reader = thread::spawn(move || {
            use std::io::Read;
            let result = stdout.read_to_end(&mut Vec::new());
            let _ = sent.send(result);
        });
        let foreign = spawn("unrelated", &foreign_pid);
        let deadline = Instant::now() + Duration::from_secs(5);
        while [&owned_pid, &foreign_pid].iter().any(|path| {
            fs::read_to_string(path)
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                .is_none()
        }) {
            assert!(
                Instant::now() < deadline,
                "session workers did not become ready"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let grandchild: u32 = fs::read_to_string(&owned_pid).unwrap().parse().unwrap();
        let stat = fs::read_to_string(format!("/proc/{grandchild}/stat")).unwrap();
        let (group, session) = super::process_session(&stat).unwrap();
        assert_eq!(session, owned.0.id());
        assert_ne!(
            group, session,
            "fixture must create an independent descendant PGID"
        );
        if leader_exits {
            let deadline = Instant::now() + Duration::from_secs(5);
            while super::poll_scenario(&mut owned.0).unwrap().is_none() {
                assert!(Instant::now() < deadline, "session leader did not exit");
                thread::sleep(Duration::from_millis(10));
            }
        } else {
            super::kill_process_tree(&mut owned.0).unwrap();
            owned.0.wait().unwrap();
        }
        owned.1 = true;
        received
            .recv_timeout(Duration::from_secs(3))
            .expect("descendant kept scenario stdout open")
            .unwrap();
        reader.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let gone = fs::read_to_string(format!("/proc/{grandchild}/stat"))
                .map(|value| {
                    value.rsplit_once(')').unwrap().1.split_whitespace().next() == Some("Z")
                })
                .unwrap_or(true);
            if gone {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "new-PGID descendant survived scenario timeout"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            super::process_state(
                &fs::read_to_string(format!("/proc/{}/stat", foreign.0.id())).unwrap()
            ) != Some("Z"),
            "unrelated session must survive"
        );
    }

    fn assert_streams_redacted(contents: &str) {
        let secrets = file_secret_variants(contents.to_owned());
        let trimmed = contents.trim_end_matches(['\r', '\n']);
        let stdout = format!("stdout-before:{trimmed}:stdout-after");
        let stderr = format!("stderr-before:{contents}:stderr-after");

        assert_eq!(
            String::from_utf8(redact(stdout.as_bytes(), &secrets)).unwrap(),
            "stdout-before:<redacted>:stdout-after"
        );
        assert_eq!(
            String::from_utf8(redact(stderr.as_bytes(), &secrets)).unwrap(),
            "stderr-before:<redacted>:stderr-after"
        );
    }

    #[test]
    fn file_secret_with_lf_tail_is_redacted_from_stdout_and_stderr() {
        assert_streams_redacted("-----BEGIN TEST KEY-----\nlf-secret\n-----END TEST KEY-----\n");
    }

    #[test]
    fn file_secret_with_crlf_tail_is_redacted_from_stdout_and_stderr() {
        assert_streams_redacted(
            "-----BEGIN TEST KEY-----\r\ncrlf-secret\r\n-----END TEST KEY-----\r\n",
        );
    }

    #[test]
    fn empty_file_secret_variants_are_skipped() {
        assert!(file_secret_variants(String::new()).is_empty());
        assert!(file_secret_variants("\r\n".to_owned()).is_empty());
    }
}
