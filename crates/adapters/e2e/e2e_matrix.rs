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
        if let Some(status) = child
            .try_wait()
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
    secrets.extend(scenario.redact_file_env.iter().filter_map(|name| {
        let path = env::var_os(name)?;
        fs::read_to_string(path).ok()
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
fn kill_process_tree(child: &mut std::process::Child) -> io::Result<()> {
    let group = format!("-{}", child.id());
    let status = Command::new("kill")
        .args(["-TERM", "--", &group])
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        return child.kill();
    }
    thread::sleep(Duration::from_millis(250));
    if child.try_wait()?.is_none() {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &group])
            .stdin(Stdio::null())
            .status();
    }
    Ok(())
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
